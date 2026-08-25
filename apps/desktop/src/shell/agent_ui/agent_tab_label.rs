//! Monotonic per-adapter counter for agent-tab labels.
//!
//! Renders a label like `"Claude #1"` / `"Codex #2"` / `"Pi #1"` from the
//! adapter slug (`"claude-code"` / `"codex"` / `"pi"`) plus a counter
//! seeded by walking the current `WorkspaceTab` list. Monotonic — closing
//! `Claude #1` and reopening will produce `Claude #2`, never `#1` again, so
//! the user's tab-history mental model stays unambiguous within one session.

use gpui::SharedString;

/// Map an adapter id slug to its dropdown display name. Mirrors the
/// `CliAgentAdapter::name()` strings the registry surfaces — duplicated
/// here so the label generator doesn't need to round-trip through the
/// registry just for the prefix. If a new adapter ships, add the row.
///
/// TODO(step-10): unify with `RegistryEntry::display_name` — the popover
/// already has the entry in hand at adapter-pick time, so pass the name
/// directly into the label function and drop this match arm.
fn display_name_for(adapter_id: &str) -> &'static str {
    match adapter_id {
        "claude-code" => "Claude",
        "codex" => "Codex",
        "pi" => "Pi",
        "omp" => "omp",
        "custom" => "Custom",
        // Unknown adapter slug — fall back to the slug itself in title case
        // would require allocation; ship a stable placeholder instead. The
        // registry contract guarantees only the four known ids today.
        _ => "Agent",
    }
}

/// Compute the next agent-tab label by scanning existing labels for the
/// same prefix and emitting `prefix + " #" + (max+1)`. O(n) over the tab
/// strip; n is small (the user opens a handful of tabs).
///
/// `existing` is the list of every current tab label, agent or not — the
/// scan filters by prefix so terminal labels (`"Terminal 1"`) don't
/// confuse the agent counter.
pub fn next_label_for(adapter_id: &str, existing: &[SharedString]) -> SharedString {
    let name = display_name_for(adapter_id);
    let prefix = format!("{name} #");
    let max_n = existing
        .iter()
        .filter_map(|s| s.strip_prefix(&prefix).and_then(|n| n.parse::<u64>().ok()))
        .max()
        .unwrap_or(0);
    SharedString::from(format!("{name} #{}", max_n + 1))
}

/// Test-only window into the private label table, so the table-driven
/// non-fallback sweep in `agent_presentation` can cover it without widening
/// the runtime API.
#[cfg(test)]
pub(crate) fn display_name_for_test(adapter_id: &str) -> &'static str {
    display_name_for(adapter_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_for_known_slugs() {
        assert_eq!(display_name_for("claude-code"), "Claude");
        assert_eq!(display_name_for("codex"), "Codex");
        assert_eq!(display_name_for("pi"), "Pi");
        assert_eq!(display_name_for("custom"), "Custom");
    }

    #[test]
    fn display_name_for_unknown_falls_back_to_agent() {
        assert_eq!(display_name_for("future-adapter"), "Agent");
    }

    #[test]
    fn first_label_starts_at_one() {
        let label = next_label_for("claude-code", &[]);
        assert_eq!(label.as_ref(), "Claude #1");
    }

    #[test]
    fn counter_skips_existing_numbers() {
        let existing = vec![
            SharedString::from("Claude #1"),
            SharedString::from("Claude #2"),
        ];
        let label = next_label_for("claude-code", &existing);
        assert_eq!(label.as_ref(), "Claude #3");
    }

    #[test]
    fn counter_is_monotonic_after_close() {
        // Closing #1 then asking for a new label still returns #3 because
        // #2 is still on the strip. This is the intentional "monotonic
        // within session" contract.
        let existing = vec![SharedString::from("Claude #2")];
        let label = next_label_for("claude-code", &existing);
        assert_eq!(label.as_ref(), "Claude #3");
    }

    #[test]
    fn unrelated_terminal_labels_are_ignored() {
        let existing = vec![
            SharedString::from("Terminal 1"),
            SharedString::from("Terminal 2"),
            SharedString::from("Codex #5"),
        ];
        // Different adapter — Codex labels should not bump the Claude counter.
        let label = next_label_for("claude-code", &existing);
        assert_eq!(label.as_ref(), "Claude #1");
    }

    #[test]
    fn different_adapters_have_independent_counters() {
        let existing = vec![
            SharedString::from("Claude #3"),
            SharedString::from("Codex #1"),
        ];
        let next_claude = next_label_for("claude-code", &existing);
        let next_codex = next_label_for("codex", &existing);
        assert_eq!(next_claude.as_ref(), "Claude #4");
        assert_eq!(next_codex.as_ref(), "Codex #2");
    }

    #[test]
    fn non_numeric_suffix_is_ignored() {
        // A label like "Claude #abc" shouldn't crash or trick the counter.
        let existing = vec![SharedString::from("Claude #abc")];
        let label = next_label_for("claude-code", &existing);
        assert_eq!(label.as_ref(), "Claude #1");
    }
}
