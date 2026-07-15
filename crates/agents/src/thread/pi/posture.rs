//! Pi's session-level tool posture — the ONLY real gate on what pi can do.
//!
//! Pi has **no per-call tool approval**: nothing in its protocol asks before a
//! tool runs, and a live probe watched `bash` execute a 60-iteration shell loop
//! with zero round-trips. So gating is coarse and fixed at spawn — an allowlist
//! on the command line, chosen once per session.
//!
//! This is a genuine difference from the chat surface's other backends, which do
//! ask: a Claude chat spawns with `--permission-mode` and renders approval cards.
//! **Pi cannot, so the posture must be visible in the UI** — a chat that silently
//! auto-runs `bash` beside chats that ask is the failure mode this module exists
//! to prevent. Never render a per-call approval affordance for pi; there is
//! nothing behind it.

/// Wire values for the tools posture. Raw strings (not a typed enum at the
/// `ConnectSpec` boundary) to match the Codex posture precedent, which carries
/// its approval/sandbox choices as wire strings straight from the picker.
pub const TOOLS_STANDARD: &str = "standard";
pub const TOOLS_READ_ONLY: &str = "read-only";
pub const TOOLS_NONE: &str = "none";

/// Feature-control ids echoed back by the composer.
pub const FEATURE_TOOLS: &str = "pi_tools";
pub const FEATURE_CONTEXT_FILES: &str = "pi_context_files";

/// Pi's built-in tools, verified against upstream source
/// (`core/tools/index.ts`: `type ToolName = "read" | "bash" | "edit" | "write" |
/// "grep" | "find" | "ls"`). Read-only is exactly this set minus the three that
/// can change the machine.
const READ_ONLY_TOOLS: &str = "read,find,grep,ls";

/// One Pi session's posture, fixed at spawn.
// Serializable so a restored chat reconnects under the posture the user chose,
// rather than silently reverting to the (more permissive) default.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PiPosture {
    /// One of [`TOOLS_STANDARD`] / [`TOOLS_READ_ONLY`] / [`TOOLS_NONE`].
    pub tools: String,
    /// Whether pi auto-loads `AGENTS.md`/`CLAUDE.md` into context. `true`
    /// (pi's default) unless the user opts out.
    pub context_files: bool,
}

impl Default for PiPosture {
    /// **Standard, deliberately** — a user decision (2026-07-15), not an
    /// accident of `Option::None`.
    ///
    /// No `--tools` flag means pi's own default (`read, bash, edit, write`),
    /// which **auto-run**. That matches the `pi` CLI, pi-gui, and the `pi`
    /// terminal agent this app already ships, so it grants no capability the
    /// user doesn't already have — but it does mean a Pi chat runs commands
    /// where a sibling Claude chat would ask. The posture control is the
    /// mitigation; it is not optional.
    fn default() -> Self {
        Self { tools: TOOLS_STANDARD.to_string(), context_files: true }
    }
}

impl PiPosture {
    /// Rebuild from persisted/composer values, falling back to the default for
    /// anything absent or unrecognised — a posture we can't parse must never
    /// silently become *more* permissive than the user's last choice, and
    /// Standard is already the most permissive, so an unknown value is clamped
    /// to it only because that IS the default. An unknown value is logged.
    pub fn from_parts(tools: Option<&str>, context_files: Option<bool>) -> Self {
        let tools = match tools {
            Some(t) if matches!(t, TOOLS_STANDARD | TOOLS_READ_ONLY | TOOLS_NONE) => t.to_string(),
            Some(other) => {
                tracing::warn!(posture = %other, "unknown pi tools posture; using the default");
                TOOLS_STANDARD.to_string()
            }
            None => TOOLS_STANDARD.to_string(),
        };
        Self { tools, context_files: context_files.unwrap_or(true) }
    }

    /// The spawn flags this posture implies.
    ///
    /// Never emits `--approve`/`-a`: that flag is unrelated to tool gating (it
    /// trusts project-local extension/skill files) and pi's existing trust state
    /// is the user's own, scoped to a parent directory. Widening it is not this
    /// app's call.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        match self.tools.as_str() {
            TOOLS_READ_ONLY => {
                args.push("--tools".to_string());
                args.push(READ_ONLY_TOOLS.to_string());
            }
            TOOLS_NONE => args.push("--no-tools".to_string()),
            // Standard passes nothing — pi's default is read/bash/edit/write.
            _ => {}
        }
        if !self.context_files {
            args.push("--no-context-files".to_string());
        }
        args
    }

    /// Whether this posture lets pi change the machine (bash/edit/write). Drives
    /// the UI's warning affordance.
    pub fn can_mutate(&self) -> bool {
        self.tools == TOOLS_STANDARD
    }

    /// Short human label for the posture pill.
    pub fn label(&self) -> &'static str {
        match self.tools.as_str() {
            TOOLS_READ_ONLY => "Read-only",
            TOOLS_NONE => "No tools",
            _ => "Auto-run",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_standard_and_deliberate() {
        // Recorded user decision. If this ever changes, it must change here —
        // explicitly — rather than by a caller passing None somewhere.
        let p = PiPosture::default();
        assert_eq!(p.tools, TOOLS_STANDARD);
        assert!(p.context_files, "pi loads AGENTS.md by default; we match the CLI");
        assert!(p.can_mutate(), "Standard auto-runs bash/edit/write — that is the point of the pill");
        assert!(p.to_args().is_empty(), "standard passes no flags: pi's own default applies");
    }

    #[test]
    fn read_only_removes_exactly_bash_edit_and_write() {
        // pi's built-ins are read|bash|edit|write|grep|find|ls (upstream
        // core/tools/index.ts). Read-only must keep the four harmless ones.
        let p = PiPosture { tools: TOOLS_READ_ONLY.into(), context_files: true };
        assert_eq!(p.to_args(), vec!["--tools", "read,find,grep,ls"]);
        assert!(!p.can_mutate());
        for allowed in ["read", "find", "grep", "ls"] {
            assert!(READ_ONLY_TOOLS.contains(allowed), "{allowed} must stay available");
        }
        for denied in ["bash", "edit", "write"] {
            assert!(!READ_ONLY_TOOLS.split(',').any(|t| t == denied), "{denied} must be gone");
        }
    }

    #[test]
    fn no_tools_disables_everything() {
        let p = PiPosture { tools: TOOLS_NONE.into(), context_files: true };
        assert_eq!(p.to_args(), vec!["--no-tools"]);
        assert!(!p.can_mutate());
    }

    #[test]
    fn opting_out_of_context_files_adds_the_flag_to_any_posture() {
        // AGENTS.md auto-load is a prompt-injection path into an ungated tool
        // loop. Inherited from pi, but the opt-out must be reachable.
        let p = PiPosture { tools: TOOLS_STANDARD.into(), context_files: false };
        assert_eq!(p.to_args(), vec!["--no-context-files"]);
        let p = PiPosture { tools: TOOLS_READ_ONLY.into(), context_files: false };
        assert_eq!(p.to_args(), vec!["--tools", "read,find,grep,ls", "--no-context-files"]);
    }

    #[test]
    fn approve_is_never_passed_by_any_posture() {
        // `--approve` trusts project-local extension/skill files — unrelated to
        // tool gating, and pi's trust state is parent-directory scoped
        // (one grant covers every sibling repo). Never widen it from here.
        for tools in [TOOLS_STANDARD, TOOLS_READ_ONLY, TOOLS_NONE] {
            for context_files in [true, false] {
                let args = PiPosture { tools: tools.into(), context_files }.to_args();
                assert!(
                    !args.iter().any(|a| a == "--approve" || a == "-a"),
                    "posture {tools} must not pass --approve, got {args:?}"
                );
            }
        }
    }

    #[test]
    fn an_unrecognised_posture_falls_back_to_the_default() {
        let p = PiPosture::from_parts(Some("bogus"), None);
        assert_eq!(p.tools, TOOLS_STANDARD);
        let p = PiPosture::from_parts(Some(TOOLS_READ_ONLY), Some(false));
        assert_eq!(p.tools, TOOLS_READ_ONLY);
        assert!(!p.context_files);
        // Absent = default.
        assert_eq!(PiPosture::from_parts(None, None), PiPosture::default());
    }

    #[test]
    fn labels_are_honest_about_auto_running() {
        assert_eq!(PiPosture::default().label(), "Auto-run");
        assert_eq!(PiPosture { tools: TOOLS_READ_ONLY.into(), context_files: true }.label(), "Read-only");
        assert_eq!(PiPosture { tools: TOOLS_NONE.into(), context_files: true }.label(), "No tools");
    }
}
