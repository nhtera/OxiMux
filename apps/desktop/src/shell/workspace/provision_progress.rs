//! The provisioning card's model: one create's progress, and the one line
//! formatter the transcript file and the card share.
//!
//! No GPUI in here, so every rule — when a card reveals, what a failure
//! does, how the tail is bounded — is a plain unit test. The layer that
//! paints it lives in `provision_card.rs`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use oximux_worktree_ops::{ProvisionEvent, SetupOutcome};

/// How long provisioning must have been running before a card appears.
pub const SHOW_AFTER: Duration = Duration::from_millis(600);
/// Lines kept in memory per create. A setup script can emit megabytes; the
/// file keeps everything, the card keeps a tail.
pub const MAX_LINES: usize = 200;

/// The one line an event becomes, in the transcript file and on the card.
pub fn provision_line(event: &ProvisionEvent) -> String {
    match event {
        ProvisionEvent::IncludeCopied(p) => format!("include: copied {}", p.display()),
        ProvisionEvent::IncludeSkipped(skip) => format!("include: skipped {skip}"),
        ProvisionEvent::FreshenStarted(branch) => format!("fetching {branch}\u{2026}"),
        ProvisionEvent::FreshenFinished(summary) => format!("== {summary}"),
        ProvisionEvent::SetupSkipped(reason) => format!("== {reason}"),
        ProvisionEvent::SetupStarted(script) => format!("$ {script}"),
        ProvisionEvent::SetupLine(line) => line.clone(),
        ProvisionEvent::SetupFinished(outcome) => format!("== {}", outcome.summary()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionState {
    Running,
    Finished,
    Failed { summary: String },
}

/// One create's progress.
#[derive(Debug)]
pub struct ProvisionProgress {
    pub id: u64,
    pub slug: String,
    /// The project the create belongs to — where `Open transcript` opens its
    /// tab, even if the user has switched projects since.
    pub project_id: String,
    /// The durable record, for the failure card's `Open transcript`.
    pub transcript: PathBuf,
    lines: VecDeque<String>,
    pub state: ProvisionState,
    /// Whether the card paints. Set by the [`SHOW_AFTER`] timer while still
    /// running, by `SetupStarted`, or by a failure.
    pub visible: bool,
}

impl ProvisionProgress {
    pub fn new(id: u64, slug: String, project_id: String, transcript: PathBuf) -> Self {
        Self {
            id,
            slug,
            project_id,
            transcript,
            lines: VecDeque::new(),
            state: ProvisionState::Running,
            visible: false,
        }
    }

    /// Record one event. `SetupStarted` reveals the card at once: a setup
    /// script is the thing that makes a create slow.
    pub fn push_event(&mut self, event: &ProvisionEvent) {
        if matches!(event, ProvisionEvent::SetupStarted(_)) {
            self.visible = true;
        }
        if self.lines.len() == MAX_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(provision_line(event));
    }

    /// The last `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> impl Iterator<Item = &String> {
        self.lines.iter().skip(self.lines.len().saturating_sub(n))
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The [`SHOW_AFTER`] timer fired: reveal iff still running. Returns
    /// whether anything changed.
    pub fn reveal_if_running(&mut self) -> bool {
        if self.state == ProvisionState::Running && !self.visible {
            self.visible = true;
            return true;
        }
        false
    }

    /// The create ended. A failure always shows — the tail is what explains
    /// it — even if the create was fast enough never to have appeared. A
    /// second terminal outcome is ignored: the first one is the truth.
    pub fn finish(&mut self, outcome: Result<(), String>) -> bool {
        if self.state != ProvisionState::Running {
            return false;
        }
        self.state = match outcome {
            Ok(()) => ProvisionState::Finished,
            Err(summary) => {
                self.visible = true;
                ProvisionState::Failed { summary }
            }
        };
        true
    }

    pub fn is_running(&self) -> bool {
        self.state == ProvisionState::Running
    }
}

/// The card's outcome for a setup result — the setup half of a create's
/// outcome; git and storage failures map to `Err` at the call site.
pub fn card_outcome_for_setup(outcome: &SetupOutcome) -> Result<(), String> {
    match outcome {
        SetupOutcome::Ok => Ok(()),
        other => Err(other.summary()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_worktree_ops::include::Skip;

    fn progress() -> ProvisionProgress {
        ProvisionProgress::new(1, "amber".into(), "p-1".into(), PathBuf::from("/t/amber.log"))
    }

    /// Each of the seven skip reasons reads as a distinct sentence — a user
    /// whose pattern matched nothing learns it here, not an hour later.
    #[test]
    fn every_skip_variant_renders_a_distinct_reason() {
        let skips = [
            Skip::AlreadyPresent(PathBuf::from(".env")),
            Skip::SourceIsSymlink(PathBuf::from("link")),
            Skip::TargetPathCrossesSymlink(PathBuf::from("dir/x")),
            Skip::MatchedNothing("*.pem".into()),
            Skip::EscapesProjectRoot("../x".into()),
            Skip::Failed {
                path: PathBuf::from("big.bin"),
                error: "permission denied".into(),
            },
            Skip::ScanBudgetExhausted("**/*".into()),
        ];
        let lines: Vec<String> = skips
            .iter()
            .map(|s| provision_line(&ProvisionEvent::IncludeSkipped(s.clone())))
            .collect();
        let distinct: std::collections::HashSet<&String> = lines.iter().collect();
        assert_eq!(distinct.len(), 7, "{lines:#?}");
        for line in &lines {
            assert!(line.starts_with("include: skipped "), "{line}");
            assert!(line.len() > "include: skipped ".len() + 8, "too terse: {line}");
        }
        assert!(lines[3].contains("matched nothing"), "{}", lines[3]);
    }

    #[test]
    fn the_buffer_is_bounded_and_the_tail_is_the_newest() {
        let mut p = progress();
        for i in 0..(MAX_LINES + 50) {
            p.push_event(&ProvisionEvent::SetupLine(format!("line {i}")));
        }
        assert_eq!(p.line_count(), MAX_LINES);
        let tail: Vec<&String> = p.tail(3).collect();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[2], &format!("line {}", MAX_LINES + 49));
    }

    /// Hidden until the timer says so — unless setup starts, which reveals
    /// at once.
    #[test]
    fn reveal_rules() {
        let mut p = progress();
        p.push_event(&ProvisionEvent::IncludeCopied(PathBuf::from(".env")));
        assert!(!p.visible, "an include copy alone does not show a card");
        assert!(p.reveal_if_running(), "the timer reveals a running create");
        assert!(p.visible);
        assert!(!p.reveal_if_running(), "idempotent");

        let mut fast = progress();
        assert!(fast.finish(Ok(())));
        assert!(!fast.reveal_if_running(), "a create that already finished never appears");
        assert!(!fast.visible);

        let mut slow = progress();
        slow.push_event(&ProvisionEvent::SetupStarted("pnpm install".into()));
        assert!(slow.visible, "setup starting reveals immediately");
    }

    #[test]
    fn failure_always_shows_and_the_first_outcome_wins() {
        let mut p = progress();
        assert!(p.finish(Err("setup exited 7".into())));
        assert!(p.visible);
        assert_eq!(
            p.state,
            ProvisionState::Failed {
                summary: "setup exited 7".into()
            }
        );
        assert!(!p.is_running());
        // A later, contradictory outcome is ignored.
        assert!(!p.finish(Ok(())));
        assert!(matches!(p.state, ProvisionState::Failed { .. }));
    }

    #[test]
    fn setup_outcome_maps_to_the_card_outcome() {
        assert_eq!(card_outcome_for_setup(&SetupOutcome::Ok), Ok(()));
        assert!(card_outcome_for_setup(&SetupOutcome::NonZero { code: Some(7) })
            .unwrap_err()
            .contains("7"));
        assert!(card_outcome_for_setup(&SetupOutcome::TimedOut).is_err());
    }

    #[test]
    fn the_file_and_the_card_share_one_formatter() {
        // The transcript writer calls `provision_line`; this pins the shapes
        // the file has always had so the tee cannot drift them.
        assert_eq!(
            provision_line(&ProvisionEvent::SetupStarted("make".into())),
            "$ make"
        );
        assert_eq!(
            provision_line(&ProvisionEvent::SetupFinished(SetupOutcome::Ok)),
            "== setup succeeded"
        );
        assert_eq!(
            provision_line(&ProvisionEvent::IncludeCopied(PathBuf::from(".env"))),
            "include: copied .env"
        );
    }
}
