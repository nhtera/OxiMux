//! The restore marker, kept in view for a CLI that erases it.
//!
//! A cold-restored agent tab gets its marker ("resuming previous session",
//! "previous session unavailable, started fresh") prefilled into the grid
//! before the CLI's first byte. Several CLIs erase it, each differently —
//! measured: omp sends `ESC[2J ESC[3J` with its first frame; Pi sends the same
//! on every resize; Claude Code, on a resize, homes the cursor and erases
//! every visible line (`ESC[H` + one `ESC[2K` per row), which leaves the
//! scrollback alone and so raises no wipe event at all. A background tab gets
//! its first resize whenever the user first opens it. Writing the marker back
//! into the grid is not an option: the CLI owns the screen and diffs against
//! what it drew, so injected text would be torn by its next repaint.
//!
//! So the words move off the grid instead. The pane arms a notice with the
//! marker's text when it prefills it. Every erase measured follows a resize or
//! a scrollback wipe, so only those schedule a check: `SETTLE` after the last
//! of a burst (the CLI's repaint lands first), the whole buffer is searched
//! once for the marker — framed and dim, so a transcript that merely quotes
//! the words does not count. Once it is gone the same text shows as a strip
//! over the pane until the user's first input. Plain output never triggers a
//! search, so a busy tab costs nothing; and the notice lives for `WINDOW`
//! from the first such event (a background tab's first resize is when it is
//! first shown), so a marker that later scrolls out of history, or a resize an
//! hour on, never re-announces a restore long past. A CLI that leaves the
//! marker alone (Codex) never shows the strip.

use super::*;

/// Quiet time after the last resize/wipe before the buffer is searched, so
/// the CLI's repaint in answer to the resize has landed.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(750);

/// How long after its first resize/wipe the notice keeps watching.
const WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) struct RestoreNotice {
    label: &'static str,
    shown: bool,
    /// Set by the first resize/wipe after arming; the notice retires at
    /// `WINDOW` past it.
    first_event: Option<std::time::Instant>,
    /// A search is due at this instant (pushed back by every new event).
    check_at: Option<std::time::Instant>,
}

/// What a search of the buffer found.
#[derive(Debug, PartialEq, Eq)]
enum Search {
    Present,
    Gone,
    /// The backend returned no grid at all (a backend that keeps no
    /// scrollback, an unknown session): no evidence either way.
    Unknown,
}

/// Search `grid` (history + visible, row-major) for the framed marker
/// `--- {label} ---` drawn dim, as the prefill draws it. Row-wise: a pane
/// narrower than the marker wraps it and reads as gone, which only costs a
/// redundant strip.
fn find_marker(grid: &[Vec<oximux_pty::Cell>], label: &str) -> Search {
    if grid.is_empty() {
        return Search::Unknown;
    }
    let framed: Vec<char> = format!("--- {label} ---").chars().collect();
    let found = grid.iter().any(|row| {
        row.windows(framed.len())
            .any(|w| w[0].dim && w.iter().zip(&framed).all(|(c, f)| c.ch == *f))
    });
    if found { Search::Present } else { Search::Gone }
}

impl TerminalView {
    /// Arm the off-grid notice for the restore marker just prefilled into this
    /// pane. Replaces any earlier one: the resume fallback prefills a second
    /// marker ("started fresh") that supersedes the first.
    pub fn arm_restore_notice(&mut self, label: &'static str) {
        self.restore_notice = Some(RestoreNotice {
            label,
            shown: false,
            first_event: None,
            check_at: None,
        });
    }

    /// The pane was resized or its scrollback wiped — the only events every
    /// measured erase follows. Schedules one search once the burst settles.
    pub(super) fn note_erase_risk_for_notice(&mut self) {
        let now = std::time::Instant::now();
        if let Some(notice) = self.restore_notice.as_mut().filter(|n| !n.shown) {
            notice.first_event.get_or_insert(now);
            notice.check_at = Some(now + SETTLE);
        }
    }

    /// Run a due search, and retire a notice whose window has passed. Called
    /// every tick. `true` when the notice just became visible.
    pub(super) fn recheck_restore_notice(&mut self) -> bool {
        let now = std::time::Instant::now();
        let Some(notice) = self.restore_notice.as_ref().filter(|n| !n.shown) else {
            return false;
        };
        if notice.first_event.is_some_and(|t| now.saturating_duration_since(t) > WINDOW) {
            self.restore_notice = None;
            return false;
        }
        if notice.check_at.is_none_or(|t| now < t) {
            return false;
        }
        let label = notice.label;
        let id = self.session_id;
        let found = find_marker(&self.with_backend(|be| be.search_grid(id)), label);
        let Some(notice) = self.restore_notice.as_mut() else {
            return false;
        };
        notice.check_at = None;
        notice.shown = found == Search::Gone;
        notice.shown
    }

    /// The user sent input: the notice has done its job. `true` when a
    /// visible one was removed, so the caller repaints.
    pub(super) fn dismiss_restore_notice(&mut self) -> bool {
        self.restore_notice.take().is_some_and(|n| n.shown)
    }

    /// The notice's text while it is showing.
    pub(super) fn visible_restore_notice(&self) -> Option<&'static str> {
        self.restore_notice.as_ref().filter(|n| n.shown).map(|n| n.label)
    }
}

/// Centered top strip carrying the restore marker's words, styled like the
/// exit banner but dimmer: it is information, not a state change.
pub(super) fn build_restore_notice(theme: &Theme, label: &str, density: Density, typo: &Typography) -> gpui::Div {
    div()
        .absolute()
        .top(px(6.0))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .px(px(10.0))
                .py(px(3.0))
                .rounded(px(density.r_xs))
                .bg(theme.bg_overlay)
                .text_color(theme.fg_muted)
                .text_size(px(typo.t_body_sm))
                .border_1()
                .border_color(theme.border_inactive)
                .child(format!("--- {label} ---")),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str, dim: bool) -> Vec<oximux_pty::Cell> {
        text.chars()
            .map(|ch| oximux_pty::Cell { ch, dim, ..Default::default() })
            .collect()
    }

    const LABEL: &str = "resuming previous session";

    #[test]
    fn the_dim_framed_marker_is_found_in_any_row() {
        let grid = vec![row("$ ls", false), row("--- resuming previous session ---", true), row("omp", false)];
        assert_eq!(find_marker(&grid, LABEL), Search::Present);
    }

    #[test]
    fn a_wiped_buffer_reads_gone_and_an_empty_one_unknown() {
        assert_eq!(find_marker(&[row("omp v18", false), row("Tips", false)], LABEL), Search::Gone);
        assert_eq!(find_marker(&[], LABEL), Search::Unknown);
    }

    #[test]
    fn quoted_words_do_not_count_as_the_marker() {
        // A replayed transcript that talks about the feature: same words,
        // not framed, or framed but not drawn dim.
        let grid = vec![
            row("the tab shows resuming previous session on restore", false),
            row("--- resuming previous session ---", false),
        ];
        assert_eq!(find_marker(&grid, LABEL), Search::Gone);
    }
}
