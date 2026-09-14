//! Live provisioning progress — a floating card per in-flight worktree
//! create, fed by the same event stream the transcript file is written from.
//!
//! Provisioning (`.oximuxinclude` copy, default-branch freshen, the setup
//! script) already streams [`ProvisionEvent`]s; until now the desktop wrote
//! them to a file and showed nothing until the create ended. This layer
//! draws that stream as it happens.
//!
//! Placement is a **floating card**, not a rail-row attachment, because the
//! `workspaces` row is inserted *last* — the rail never lists a half-built
//! worktree, and that ordering is deliberate. Cards stack bottom-LEFT so they
//! never collide with the toast stack (bottom-right).
//!
//! Two rules keep it quiet: a card appears only once provisioning has run
//! longer than [`SHOW_AFTER`] (a create with no setup script finishes well
//! inside that and shows nothing), or immediately on `SetupStarted`, which
//! is itself the signal that this create will be slow. And dismissing a card
//! never cancels the create — the card is a view of the work, not the work.
//!
//! One formatter, [`provision_line`], serves both the file and the card, so
//! the two can never disagree about what happened.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    AnyElement, ClickEvent, Context, ElementId, IntoElement, ParentElement, Render, Styled,
    Window, div, px,
};
use gpui::prelude::FluentBuilder;
use gpui_component::Sizable;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::spinner::Spinner;
use oximux_settings::{Density, Theme, Typography};
use oximux_worktree_ops::{ProvisionEvent, SETUP_TIMEOUT, SetupOutcome};

use crate::ui::FloatingSurface;

/// How long provisioning must have been running before a card appears.
pub const SHOW_AFTER: Duration = Duration::from_millis(600);
/// Lines kept in memory per create. A setup script can emit megabytes; the
/// file keeps everything, the card keeps a tail.
pub const MAX_LINES: usize = 200;
/// Lines the card paints.
const TAIL_LINES: usize = 8;
/// How long a successful card lingers before it goes.
const LINGER_AFTER_SUCCESS: Duration = Duration::from_secs(3);
/// A create that has produced no terminal state past the setup cap plus a
/// margin is marked failed rather than spinning forever.
const HARD_TIMEOUT: Duration = Duration::from_secs(SETUP_TIMEOUT.as_secs() + 120);

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

/// One create's progress. No GPUI in here, so the rules are unit-testable.
#[derive(Debug)]
pub struct ProvisionProgress {
    pub id: u64,
    pub slug: String,
    /// The durable record, for the failure card's `Open transcript`.
    pub transcript: PathBuf,
    lines: VecDeque<String>,
    pub state: ProvisionState,
    /// Whether the card paints. Set by the [`SHOW_AFTER`] timer while still
    /// running, by `SetupStarted`, or by a failure.
    pub visible: bool,
}

impl ProvisionProgress {
    fn new(id: u64, slug: String, transcript: PathBuf) -> Self {
        Self {
            id,
            slug,
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
    /// it — even if the create was fast enough never to have appeared.
    pub fn finish(&mut self, outcome: Result<(), String>) {
        self.state = match outcome {
            Ok(()) => ProvisionState::Finished,
            Err(summary) => {
                self.visible = true;
                ProvisionState::Failed { summary }
            }
        };
    }

    pub fn is_running(&self) -> bool {
        self.state == ProvisionState::Running
    }
}

/// The per-window stack of provisioning cards, mounted by the workspace root
/// beside the toast layer.
pub struct ProvisionLayer {
    theme: Theme,
    density: Density,
    typography: Typography,
    entries: Vec<ProvisionProgress>,
    next_id: u64,
}

impl ProvisionLayer {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            theme,
            density,
            typography,
            entries: Vec::new(),
            next_id: 0,
        }
    }

    /// Refresh the design tokens from the workspace root each render.
    pub fn set_tokens(&mut self, theme: Theme, density: Density, typography: Typography) {
        self.theme = theme;
        self.density = density;
        self.typography = typography;
    }

    /// A create is starting. Returns the card id the create task feeds and
    /// finishes. Arms the reveal timer and the hard-timeout watchdog.
    pub fn begin(&mut self, slug: String, transcript: PathBuf, cx: &mut Context<Self>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(ProvisionProgress::new(id, slug, transcript));

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SHOW_AFTER).await;
            let _ = this.update(cx, |layer, cx| {
                if let Some(e) = layer.entry_mut(id)
                    && e.reveal_if_running()
                {
                    cx.notify();
                }
            });
        })
        .detach();

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HARD_TIMEOUT).await;
            let _ = this.update(cx, |layer, cx| {
                if layer.entry_mut(id).is_some_and(|e| e.is_running()) {
                    tracing::warn!(id, "provisioning card hit the hard timeout with no outcome");
                    layer.finish(id, Err("provisioning produced no outcome in time".into()), cx);
                }
            });
        })
        .detach();

        id
    }

    /// Record a batch of events for `id` and repaint once. The drain task
    /// batches everything queued since its last wake, so a script emitting
    /// thousands of lines a second costs one repaint per frame, not per line.
    pub fn push_events(&mut self, id: u64, events: &[ProvisionEvent], cx: &mut Context<Self>) {
        let Some(entry) = self.entry_mut(id) else {
            return; // dismissed; the create carries on regardless
        };
        for event in events {
            entry.push_event(event);
        }
        cx.notify();
    }

    /// The create ended. Success lingers briefly if the card was showing and
    /// goes at once if it never appeared; failure stays until dismissed.
    pub fn finish(&mut self, id: u64, outcome: Result<(), String>, cx: &mut Context<Self>) {
        let Some(entry) = self.entry_mut(id) else {
            return;
        };
        let was_visible = entry.visible;
        entry.finish(outcome);
        match &entry.state {
            ProvisionState::Finished if !was_visible => self.remove(id, cx),
            ProvisionState::Finished => {
                cx.notify();
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(LINGER_AFTER_SUCCESS).await;
                    let _ = this.update(cx, |layer, cx| layer.remove(id, cx));
                })
                .detach();
            }
            _ => cx.notify(),
        }
    }

    /// The user closed the card. The create is untouched: the drain and the
    /// outcome simply find no entry to update.
    pub fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.remove(id, cx);
    }

    fn remove(&mut self, id: u64, cx: &mut Context<Self>) {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        if self.entries.len() != before {
            cx.notify();
        }
    }

    fn entry_mut(&mut self, id: u64) -> Option<&mut ProvisionProgress> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Ids of the cards currently painted — for tests and for the root's
    /// "anything in flight" questions.
    pub fn visible_ids(&self) -> Vec<u64> {
        self.entries.iter().filter(|e| e.visible).map(|e| e.id).collect()
    }

    fn render_card(&self, entry: &ProvisionProgress, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let typo = &self.typography;
        let id = entry.id;
        let (accent, title): (gpui::Hsla, String) = match &entry.state {
            ProvisionState::Running => (
                theme.status_info,
                format!("Creating \u{201c}{}\u{201d}\u{2026}", entry.slug),
            ),
            ProvisionState::Finished => (
                theme.status_ok,
                format!("Created \u{201c}{}\u{201d}", entry.slug),
            ),
            ProvisionState::Failed { .. } => (
                theme.status_error,
                format!("Couldn\u{2019}t set up \u{201c}{}\u{201d}", entry.slug),
            ),
        };

        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .when(entry.is_running(), |h| {
                // gpui-component's spinner drives its own animation frames
                // (`with_animation`), which is the timer-driven shape the
                // render-tick trap forbids us from hand-rolling.
                h.child(Spinner::new().small().color(accent))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(typo.t_body_sm))
                    .font_weight(typo.w_semibold)
                    .text_color(theme.fg_base)
                    .child(title),
            );
        header = header.child(
            Button::new(ElementId::Name(format!("provision-{id}-dismiss").into()))
                .ghost()
                .xsmall()
                .label("\u{00d7}")
                .on_click(cx.listener(move |layer, _: &ClickEvent, _window, cx| {
                    layer.dismiss(id, cx);
                })),
        );

        let mut body = div().flex().flex_col().gap(px(2.0)).min_w_0();
        for line in entry.tail(TAIL_LINES) {
            body = body.child(
                div()
                    .text_size(px(typo.t_label_xs))
                    .font(typo.mono_font())
                    .text_color(theme.fg_muted)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(line.clone()),
            );
        }

        let mut card_body = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(12.0))
            .py(px(8.0))
            .child(header)
            .when(entry.line_count() > 0, |b| b.child(body));

        if let ProvisionState::Failed { summary } = &entry.state {
            let path = entry.transcript.clone();
            card_body = card_body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(typo.t_sub_label))
                            .text_color(theme.status_error)
                            .child(summary.clone()),
                    )
                    .child(
                        Button::new(ElementId::Name(format!("provision-{id}-transcript").into()))
                            .outline()
                            .xsmall()
                            .label("Open transcript")
                            .on_click(move |_: &ClickEvent, window: &mut Window, cx| {
                                window.dispatch_action(
                                    Box::new(crate::actions::OpenProvisioningTranscript {
                                        path: path.clone(),
                                    }),
                                    cx,
                                );
                            }),
                    ),
            );
        }

        div()
            .flex()
            .items_stretch()
            .w(px(420.0))
            .max_w(px(420.0))
            .floating_chrome(&theme, &self.density)
            .overflow_hidden()
            // The same 2px status-hue accent bar the toasts use.
            .child(div().w(px(2.0)).bg(accent))
            .child(card_body)
            .into_any_element()
    }
}

impl Render for ProvisionLayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        if self.entries.iter().all(|e| !e.visible) {
            return div();
        }
        let visible: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.visible)
            .map(|(i, _)| i)
            .collect();
        let mut cards = Vec::with_capacity(visible.len());
        for i in visible {
            let element = {
                // Split borrow: the card reads `self` immutably and needs
                // `cx` for its listeners; collect the entry by index first.
                let entry = &self.entries[i];
                // SAFETY of the borrow: `render_card` takes `&self` and
                // `&mut Context`, which the borrow checker allows since
                // `cx` is a separate parameter.
                self.render_card(entry, cx)
            };
            cards.push(element);
        }
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .justify_end()
            .items_start()
            .pl(px(16.0))
            .pb(px(self.density.h_status_bar + 12.0))
            .gap(px(8.0))
            .children(cards)
    }
}

/// Feed a card from the tee off the transcript writer, on the foreground so
/// the entity can be updated. Coalesced: every wake drains everything queued
/// since the last one and repaints once, so a script that emits thousands
/// of lines a second costs a repaint per frame, not per line. Ends when the
/// writer drops the tee (provisioning is over).
pub fn drain_into(
    layer: gpui::Entity<ProvisionLayer>,
    id: u64,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ProvisionEvent>,
    cx: &mut gpui::AsyncApp,
) {
    cx.spawn(async move |cx| {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while let Ok(next) = rx.try_recv() {
                batch.push(next);
            }
            // A strong entity: the update cannot fail while the app runs, and
            // a dismissed card is simply an id `push_events` no longer finds.
            layer.update(cx, |layer, cx| layer.push_events(id, &batch, cx));
        }
    })
    .detach();
}

/// A terminal outcome for the card, derived from the create's own outcome so
/// a hard git failure, a storage failure and a setup failure all reach the
/// card the same way. `None` means "nothing to say": a fast, silent success.
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
        ProvisionProgress::new(1, "amber".into(), PathBuf::from("/t/amber.log"))
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
        fast.finish(Ok(()));
        assert!(!fast.reveal_if_running(), "a create that already finished never appears");
        assert!(!fast.visible);

        let mut slow = progress();
        slow.push_event(&ProvisionEvent::SetupStarted("pnpm install".into()));
        assert!(slow.visible, "setup starting reveals immediately");
    }

    #[test]
    fn failure_always_shows_and_carries_the_summary() {
        let mut p = progress();
        p.finish(Err("setup exited 7".into()));
        assert!(p.visible);
        assert_eq!(
            p.state,
            ProvisionState::Failed {
                summary: "setup exited 7".into()
            }
        );
        assert!(!p.is_running());
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
