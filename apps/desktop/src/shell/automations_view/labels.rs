//! The words the Automations page and the Schedules settings pane both say.
//!
//! Two surfaces now render the same rows — the full-width Automations pane and
//! the narrow Settings → Schedules list. Wording that drifted between them
//! would read as two different features rather than two views of one store, so
//! every string a person reads about a schedule is minted here.
//!
//! Everything in this module is pure: no `Context`, no store, no clock beyond
//! what the caller passes in. That is what makes the phrasing testable, which
//! matters more here than usual — a schedule's next-fire line is the only
//! evidence a user has that an automation is actually armed.

use chrono::{DateTime, Local};
use gpui::Hsla;
use oximux_agents::schedule::{RunOutcome, ScheduleRun};
use oximux_settings::Theme;

/// How many characters of a schedule's prompt the card shows before eliding.
/// Long enough to recognise the task, short enough that a pasted paragraph
/// cannot push the run history off the card.
pub(crate) const PROMPT_PREVIEW_CHARS: usize = 120;

/// "next Jul 23 at 09:00" — the schedule's armed next-fire, for a person.
pub(crate) fn next_fire_label(next: DateTime<Local>) -> String {
    format!("next {}", next.format("%b %-d at %H:%M"))
}

/// Dot colour + text for a run. Pure so the wording is unit-tested.
pub(crate) fn run_summary(run: &ScheduleRun, theme: Theme) -> (Hsla, String) {
    let when = run.fired_at.format("%b %-d %H:%M");
    match run.outcome {
        RunOutcome::Ok => (theme.status_ok, format!("{when} · ran")),
        RunOutcome::Failed => {
            let why = run.detail.as_deref().unwrap_or("failed");
            (theme.status_error, format!("{when} · failed — {why}"))
        }
    }
}

/// The header's one-line census: how many automations exist and how many are
/// actually armed. Both numbers matter — "4 automations" beside a page where
/// nothing will fire is exactly the reassurance this feature must not give.
pub(crate) fn armed_summary(total: usize, enabled: usize) -> String {
    match (total, enabled) {
        (0, _) => "No automations".to_string(),
        (1, 1) => "1 automation · armed".to_string(),
        (1, _) => "1 automation · paused".to_string(),
        (n, 0) => format!("{n} automations · none armed"),
        (n, e) if e == n => format!("{n} automations · all armed"),
        (n, e) => format!("{n} automations · {e} armed"),
    }
}

/// Replace a leading home directory with `~`. Accepts either separator so a
/// Windows path shortens too; the separator already in the path is preserved
/// rather than normalised, because the string is shown, not walked.
pub(crate) fn home_abbrev(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return path.to_string();
    };
    let home = home.trim_end_matches(['/', '\\']);
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with(['/', '\\']) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// One line of a schedule's prompt, elided to [`PROMPT_PREVIEW_CHARS`].
///
/// Newlines collapse to spaces: a multi-line prompt rendered verbatim would
/// grow the card without telling the reader anything the first line doesn't.
/// Truncation counts CHARACTERS, not bytes — slicing a `String` mid-codepoint
/// panics, and prompts are free text.
pub(crate) fn prompt_preview(prompt: &str) -> String {
    let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PROMPT_PREVIEW_CHARS {
        return flat;
    }
    let head: String = flat.chars().take(PROMPT_PREVIEW_CHARS).collect();
    format!("{}…", head.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(outcome: RunOutcome, detail: Option<&str>) -> ScheduleRun {
        ScheduleRun {
            schedule_id: "s".into(),
            fired_at: Local::now(),
            outcome,
            session_id: None,
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn a_failed_run_summary_carries_its_detail() {
        let (_dot, text) = run_summary(
            &run(RunOutcome::Failed, Some("that working directory is not usable")),
            Theme::default(),
        );
        assert!(text.contains("failed"), "names the failure: {text}");
        assert!(text.contains("not usable"), "surfaces the detail: {text}");
    }

    #[test]
    fn an_ok_run_summary_reads_as_ran() {
        let (_dot, text) = run_summary(&run(RunOutcome::Ok, None), Theme::default());
        assert!(text.contains("ran"), "reads as ran: {text}");
    }

    /// A failure with no recorded detail must still say it failed rather than
    /// rendering an empty tail after the separator.
    #[test]
    fn a_detail_less_failure_still_reads_as_failed() {
        let (_dot, text) = run_summary(&run(RunOutcome::Failed, None), Theme::default());
        assert!(text.contains("failed"), "{text}");
        assert!(!text.ends_with("— "), "no dangling separator: {text}");
    }

    /// The census must never imply things will fire when nothing will — the
    /// whole point of counting armed separately from total.
    #[test]
    fn the_census_distinguishes_armed_from_merely_present() {
        assert_eq!(armed_summary(0, 0), "No automations");
        assert_eq!(armed_summary(1, 1), "1 automation · armed");
        assert_eq!(armed_summary(1, 0), "1 automation · paused");
        assert_eq!(armed_summary(3, 0), "3 automations · none armed");
        assert_eq!(armed_summary(3, 3), "3 automations · all armed");
        assert_eq!(armed_summary(3, 1), "3 automations · 1 armed");
    }

    #[test]
    fn home_abbrev_shortens_both_separators() {
        assert_eq!(home_abbrev("/Users/x/Code", Some("/Users/x")), "~/Code");
        assert_eq!(home_abbrev(r"C:\Users\x\Code", Some(r"C:\Users\x")), r"~\Code");
        assert_eq!(home_abbrev("/Users/x", Some("/Users/x")), "~");
    }

    /// A trailing separator on the home value is common (and harmless
    /// elsewhere); it must not defeat the prefix match.
    #[test]
    fn home_abbrev_tolerates_a_trailing_separator() {
        assert_eq!(home_abbrev("/Users/x/Code", Some("/Users/x/")), "~/Code");
    }

    /// A sibling directory that merely starts with the same characters is not
    /// under home — abbreviating it would name a path that does not exist.
    #[test]
    fn home_abbrev_leaves_a_lookalike_sibling_alone() {
        assert_eq!(home_abbrev("/Users/xavier/Code", Some("/Users/x")), "/Users/xavier/Code");
    }

    #[test]
    fn home_abbrev_without_a_home_is_the_path_itself() {
        assert_eq!(home_abbrev("/srv/build", None), "/srv/build");
        assert_eq!(home_abbrev("/srv/build", Some("")), "/srv/build");
    }

    #[test]
    fn a_short_prompt_survives_intact() {
        assert_eq!(prompt_preview("Triage overnight failures"), "Triage overnight failures");
    }

    #[test]
    fn a_multiline_prompt_collapses_to_one_line() {
        assert_eq!(prompt_preview("first\n\n  second\tthird "), "first second third");
    }

    #[test]
    fn a_long_prompt_is_elided() {
        let out = prompt_preview(&"a".repeat(PROMPT_PREVIEW_CHARS + 50));
        assert!(out.ends_with('…'), "{out}");
        assert_eq!(out.chars().count(), PROMPT_PREVIEW_CHARS + 1);
    }

    /// Truncation counts characters, so a multi-byte prompt cannot panic the
    /// render — the reason this helper exists instead of a `[..n]` slice.
    #[test]
    fn a_multibyte_prompt_truncates_without_panicking() {
        let out = prompt_preview(&"é".repeat(PROMPT_PREVIEW_CHARS + 10));
        assert_eq!(out.chars().count(), PROMPT_PREVIEW_CHARS + 1);
    }
}
