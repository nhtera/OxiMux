//! Provider-agnostic tool classification.
//!
//! The three chat backends describe a tool call differently: Claude names it
//! (`Bash`, `Read`, `Edit`, …); Codex renames its item types into the same
//! Claude vocabulary in its mapper (`commandExecution` → `Bash`); ACP carries a
//! freeform human `title` plus a typed `ToolKind` (`Execute`, `Read`, `Edit`,
//! …). [`ToolDetail`] collapses all three into one small archetype the renderer
//! switches on, so an ACP `Execute`, a Codex command, and a Claude `Bash` all
//! reach the same shell body instead of ACP tools falling to the generic
//! key:value card.
//!
//! The classifier is deliberately conservative: an ambiguous or unrecognized
//! signal resolves to [`ToolDetail::Unknown`] (the clean generic card) rather
//! than guessing a body that might render the wrong shape. It is pure and
//! gpui-free — the app crate calls it at render time from a `ToolCall`.

use serde_json::Value;

/// The rendering archetype a tool call belongs to, normalized across providers.
/// The renderer maps each variant to one body; unknown/ambiguous tools use the
/// generic card ([`ToolDetail::Unknown`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDetail {
    /// Running a command / code (Claude `Bash`, Codex command, ACP `Execute`).
    Shell,
    /// Reading a file or data (Claude `Read`, ACP `Read`).
    Read,
    /// Modifying a file (Claude `Edit`/`MultiEdit`, Codex `apply_patch`, ACP
    /// `Edit`/`Delete`/`Move`). Rendered as a diff card, not a tool body.
    Edit,
    /// Creating a whole file (Claude `Write`). Rendered as a diff card.
    Write,
    /// Searching over files/content (Claude `Grep`/`Glob`, ACP `Search`).
    Search,
    /// Retrieving external data by URL (Claude `WebFetch`, ACP `Fetch`).
    Fetch,
    /// A web search returning a result list (Claude `WebSearch`, Codex
    /// `web_search`). Distinct from [`Fetch`] because its body reads a query, not
    /// a URL.
    WebSearch,
    /// A subagent / delegated task (Claude `Agent`/`Task`).
    SubAgent,
    /// An MCP server tool (`mcp__<server>__<tool>`).
    Mcp,
    /// A plan / todo update (Claude `TodoWrite`). Surfaced by the plan panel, so
    /// its tool body stays generic.
    Plan,
    /// Internal reasoning surfaced as a tool (ACP `Think`) — plain text, no
    /// specialized body.
    Plain,
    /// Anything else — the clean generic key:value card.
    Unknown,
}

impl ToolDetail {
    /// Classify a tool call from its display `name`, an optional ACP `ToolKind`
    /// (as its snake_case wire string), and its `input`.
    ///
    /// The ACP kind is authoritative when present: an ACP tool's `name` is a
    /// freeform human title that can't be pattern-matched, so its typed kind is
    /// the only reliable signal. Only the ACP backend populates it — Claude and
    /// Codex leave it `None` and classify by name (Codex having already renamed
    /// its item types into Claude's vocabulary). `input` is reserved for future
    /// disambiguation and currently unused.
    pub fn classify(name: &str, acp_kind: Option<&str>, _input: &Value) -> ToolDetail {
        match acp_kind {
            Some(kind) => Self::from_acp_kind(kind),
            None => Self::from_name(name),
        }
    }

    /// Claude tool names (and the Claude-vocabulary names Codex maps into). An
    /// unrecognized name is `Unknown` — the generic card, never a wrong guess.
    fn from_name(name: &str) -> ToolDetail {
        if name.starts_with("mcp__") {
            return ToolDetail::Mcp;
        }
        match name {
            "Bash" => ToolDetail::Shell,
            "Read" => ToolDetail::Read,
            "Edit" | "MultiEdit" | "apply_patch" => ToolDetail::Edit,
            "Write" => ToolDetail::Write,
            "Grep" | "Glob" => ToolDetail::Search,
            "WebFetch" => ToolDetail::Fetch,
            // Codex names its web search `web_search`; Claude's is `WebSearch`.
            "WebSearch" | "web_search" => ToolDetail::WebSearch,
            // Codex sub-agent items pass their raw type name (the 0.144.1
            // `collabAgentToolCall.tool` field is a freeform collab-tool name, so
            // the type is the stable SubAgent signal); `sub_agent` covers the
            // no-`tool` fallback.
            "Agent" | "Task" | "sub_agent" | "collabAgentToolCall" | "subAgentActivity" => {
                ToolDetail::SubAgent
            }
            "TodoWrite" => ToolDetail::Plan,
            _ => ToolDetail::Unknown,
        }
    }

    /// ACP `ToolKind` (snake_case wire string) → archetype. `delete`/`move` are
    /// file mutations, so they classify as `Edit` (their diff/content renders in
    /// the same card). `switch_mode`, `other`, and any future kind fall to the
    /// generic card.
    fn from_acp_kind(kind: &str) -> ToolDetail {
        match kind {
            "execute" => ToolDetail::Shell,
            "read" => ToolDetail::Read,
            "edit" | "delete" | "move" => ToolDetail::Edit,
            "search" => ToolDetail::Search,
            "fetch" => ToolDetail::Fetch,
            "think" => ToolDetail::Plain,
            _ => ToolDetail::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn c(name: &str, kind: Option<&str>) -> ToolDetail {
        ToolDetail::classify(name, kind, &json!({}))
    }

    #[test]
    fn claude_names_classify_by_name() {
        assert_eq!(c("Bash", None), ToolDetail::Shell);
        assert_eq!(c("Read", None), ToolDetail::Read);
        assert_eq!(c("Edit", None), ToolDetail::Edit);
        assert_eq!(c("MultiEdit", None), ToolDetail::Edit);
        assert_eq!(c("Write", None), ToolDetail::Write);
        assert_eq!(c("Grep", None), ToolDetail::Search);
        assert_eq!(c("Glob", None), ToolDetail::Search);
        assert_eq!(c("WebFetch", None), ToolDetail::Fetch);
        assert_eq!(c("WebSearch", None), ToolDetail::WebSearch);
        assert_eq!(c("Agent", None), ToolDetail::SubAgent);
        assert_eq!(c("Task", None), ToolDetail::SubAgent);
        assert_eq!(c("TodoWrite", None), ToolDetail::Plan);
        assert_eq!(c("mcp__chrome__click", None), ToolDetail::Mcp);
    }

    #[test]
    fn codex_renamed_names_classify_like_claude() {
        // Codex maps its item types into Claude vocab before classification, plus
        // its own `web_search` / `apply_patch` spellings.
        assert_eq!(c("Bash", None), ToolDetail::Shell);
        assert_eq!(c("apply_patch", None), ToolDetail::Edit);
        assert_eq!(c("web_search", None), ToolDetail::WebSearch);
        assert_eq!(c("sub_agent", None), ToolDetail::SubAgent);
        // Sub-agent items pass their raw 0.144.1 type name (the `tool` field can
        // be populated, so the type is the stable SubAgent signal).
        assert_eq!(c("collabAgentToolCall", None), ToolDetail::SubAgent);
        assert_eq!(c("subAgentActivity", None), ToolDetail::SubAgent);
    }

    #[test]
    fn acp_kind_is_authoritative_over_the_freeform_title() {
        // An ACP tool's `name` is a human title; its kind drives classification.
        assert_eq!(c("Run the build", Some("execute")), ToolDetail::Shell);
        assert_eq!(c("Read src/main.rs", Some("read")), ToolDetail::Read);
        assert_eq!(c("Patch the parser", Some("edit")), ToolDetail::Edit);
        assert_eq!(c("Remove temp file", Some("delete")), ToolDetail::Edit);
        assert_eq!(c("Rename module", Some("move")), ToolDetail::Edit);
        assert_eq!(c("Find callers", Some("search")), ToolDetail::Search);
        assert_eq!(c("Fetch the page", Some("fetch")), ToolDetail::Fetch);
        assert_eq!(c("Reason about it", Some("think")), ToolDetail::Plain);
    }

    #[test]
    fn ambiguous_or_unknown_signals_stay_generic() {
        // No name match, no kind → generic.
        assert_eq!(c("SomeFutureTool", None), ToolDetail::Unknown);
        // ACP default/mode-switch/unknown kinds → generic (conservative).
        assert_eq!(c("Do a thing", Some("other")), ToolDetail::Unknown);
        assert_eq!(c("Switch mode", Some("switch_mode")), ToolDetail::Unknown);
        assert_eq!(c("Future kind", Some("teleport")), ToolDetail::Unknown);
    }

    #[test]
    fn acp_kind_wins_even_when_the_title_looks_like_a_claude_name() {
        // A freeform ACP title that happens to equal a Claude tool name must not
        // be reclassified by name — the kind is the truth for ACP.
        assert_eq!(c("Bash", Some("read")), ToolDetail::Read);
        assert_eq!(c("Read", Some("execute")), ToolDetail::Shell);
    }
}
