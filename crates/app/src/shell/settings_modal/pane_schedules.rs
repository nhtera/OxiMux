//! Schedules pane — create scheduled agent runs, list what is armed, and show
//! each schedule's recent run history.
//!
//! The desktop is the only producer of schedules today (no mobile RPC exists
//! yet), so this pane owns creation rather than being a read-only mirror. It
//! writes to the same [`ScheduleStore`] the scheduler ticker reads, so a
//! schedule created here fires on the next tick without any further wiring.
//!
//! **The constraint stated at the top is not decoration.** A scheduled run only
//! fires while the desktop app is running — it cannot wake a sleeping Mac or
//! relaunch a quit app. A user who sets an overnight run and quits has been
//! failed by the UI if it did not say so, which is why the banner leads the pane.
//!
//! Runs use the desktop's default agent (Settings → Agents); per-schedule agent
//! selection is a later addition. The `session_id` a run records is shown as text
//! only, never a jump link: the id is minted from a per-launch counter that
//! resets on restart, so a link could send someone to an unrelated session.

use chrono::{DateTime, Local};
use gpui::{
    AnyElement, Context, Entity, Hsla, IntoElement, ParentElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{Sizable as _, input::Input, input::InputState};
use oximux_agents::schedule::recurrence::MIN_INTERVAL_MINUTES;
use oximux_agents::schedule::{
    NewSchedule, Recurrence, RecurrenceError, RunOutcome, Schedule, ScheduleRun, describe,
};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{stepper, toggle_switch, value_chip};
use super::layout::{section_title, setting_row_desc};
use super::segmented::{Segment, segmented};

/// How many recent runs to show under each schedule.
const RUNS_SHOWN: u32 = 3;

/// Weekday labels, Monday-first to match [`Recurrence::WeeklyAt`]'s `0 = Monday`.
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Step size for the interval + minute steppers, in minutes.
const MINUTE_STEP: u32 = 5;

/// Which kind of recurrence the create form is building. Mirrors [`Recurrence`]'s
/// three cases without their values, which live in [`ScheduleDraft`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftKind {
    Interval,
    Daily,
    Weekly,
}

/// The in-progress recurrence choice for the create form. The text fields (name,
/// cwd, prompt) live in the modal's inputs; only the recurrence, which has no
/// text input, is held here.
#[derive(Clone, Copy)]
pub(crate) struct ScheduleDraft {
    pub kind: DraftKind,
    pub interval_minutes: u32,
    pub hour: u8,
    pub minute: u8,
    pub weekday: u8,
}

impl Default for ScheduleDraft {
    fn default() -> Self {
        // Daily at 09:00 is the least surprising default — an interval schedule
        // fires immediately and repeatedly, a poor thing to land on by accident.
        Self { kind: DraftKind::Daily, interval_minutes: 30, hour: 9, minute: 0, weekday: 0 }
    }
}

impl ScheduleDraft {
    /// Build the recurrence the draft describes, or the reason it is invalid. The
    /// constructors enforce the interval floor and time/weekday ranges, so the
    /// form cannot create a runaway or malformed schedule.
    pub(crate) fn to_recurrence(self) -> Result<Recurrence, RecurrenceError> {
        match self.kind {
            DraftKind::Interval => Recurrence::every_minutes(self.interval_minutes),
            DraftKind::Daily => Recurrence::daily_at(self.hour, self.minute),
            DraftKind::Weekly => Recurrence::weekly_at(self.weekday, self.hour, self.minute),
        }
    }
}

/// One schedule plus the tail of its run history, loaded together so the pane
/// never reads SQLite mid-paint.
pub(crate) struct ScheduleRow {
    pub schedule: Schedule,
    pub recent_runs: Vec<ScheduleRun>,
}

impl SettingsModal {
    /// Reload the schedule list + recent runs from the store. Called at `open()`
    /// and after every create/delete/toggle so the pane reflects the store the
    /// scheduler ticker is also reading.
    pub(super) fn reload_schedules(&mut self) {
        let schedules = self.schedule_store.list().unwrap_or_else(|err| {
            tracing::warn!(%err, "settings: could not list schedules");
            Vec::new()
        });
        let rows = schedules
            .into_iter()
            .map(|schedule| {
                let recent_runs =
                    self.schedule_store.runs(&schedule.id, RUNS_SHOWN).unwrap_or_default();
                ScheduleRow { schedule, recent_runs }
            })
            .collect();
        self.schedule_rows = rows;
    }

    /// Validate and create a schedule from the current form, then clear the form
    /// and reload. A missing field or an invalid recurrence sets the inline error
    /// and creates nothing.
    pub(super) fn submit_schedule_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = read_input(&self.sched_name_input, cx);
        let cwd = read_input(&self.sched_cwd_input, cx);
        let prompt = read_input(&self.sched_prompt_input, cx);
        if name.is_empty() || cwd.is_empty() || prompt.is_empty() {
            self.schedule_form_error =
                Some("Name, working directory, and prompt are all required.".into());
            cx.notify();
            return;
        }
        let recurrence = match self.schedule_draft.to_recurrence() {
            Ok(r) => r,
            Err(err) => {
                self.schedule_form_error = Some(err.to_string());
                cx.notify();
                return;
            }
        };
        let new = NewSchedule { name, cwd, prompt, agent_id: None, recurrence };
        match self.schedule_store.create(new, Local::now()) {
            Ok(_) => {
                self.schedule_form_error = None;
                clear_input(&self.sched_name_input, window, cx);
                clear_input(&self.sched_cwd_input, window, cx);
                clear_input(&self.sched_prompt_input, window, cx);
                self.reload_schedules();
            }
            Err(err) => {
                tracing::warn!(%err, "settings: could not create schedule");
                self.schedule_form_error = Some("Could not save the schedule.".into());
            }
        }
        cx.notify();
    }

    pub(super) fn toggle_schedule(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        if let Err(err) = self.schedule_store.set_enabled(&id, enabled, Local::now()) {
            tracing::warn!(%err, "settings: could not toggle schedule");
        }
        self.reload_schedules();
        cx.notify();
    }

    pub(super) fn delete_schedule(&mut self, id: String, cx: &mut Context<Self>) {
        if let Err(err) = self.schedule_store.delete(&id) {
            tracing::warn!(%err, "settings: could not delete schedule");
        }
        self.reload_schedules();
        cx.notify();
    }
}

fn read_input(input: &Option<Entity<InputState>>, cx: &Context<SettingsModal>) -> String {
    input.as_ref().map(|i| i.read(cx).value().trim().to_string()).unwrap_or_default()
}

fn clear_input(
    input: &Option<Entity<InputState>>,
    window: &mut Window,
    cx: &mut Context<SettingsModal>,
) {
    if let Some(i) = input {
        i.update(cx, |s, cx| s.set_value("", window, cx));
    }
}

pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(16.0))
        .child(constraint_banner(theme, typography))
        .child(create_form(modal, theme, density, typography, cx))
        .child(schedule_list(modal, theme, density, typography, cx))
        .into_any_element()
}

/// The leading disclosure: a scheduled run only fires while the app is running.
/// Read before the form, not discovered after an overnight run silently did not
/// happen.
fn constraint_banner(theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(2.0))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child("Scheduled runs only fire while OxiMux is running. Leaving it open keeps this")
        .child("Mac awake so a run is not missed — but it cannot wake a sleeping Mac or")
        .child("relaunch a quit app. Runs use your default agent (Settings → Agents).")
        .into_any_element()
}

fn create_form(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(6.0))
        .child(section_title("New schedule", "", theme, typography))
        .child(text_field("Name", &modal.sched_name_input, theme, typography))
        .child(text_field("Working directory", &modal.sched_cwd_input, theme, typography))
        .child(text_field("Prompt", &modal.sched_prompt_input, theme, typography))
        .child(setting_row_desc(
            "Repeats",
            "How often the run fires.",
            recurrence_kind_picker(modal.schedule_draft.kind, theme, density, typography, cx),
            theme,
            typography,
        ))
        .child(recurrence_editor(modal.schedule_draft, theme, density, typography, cx));

    if let Some(err) = &modal.schedule_form_error {
        col = col.child(
            div()
                .pt(px(4.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.status_error)
                .child(err.clone()),
        );
    }

    col.child(
        div().pt(px(8.0)).child(value_chip(
            "sched-create",
            "Create schedule",
            theme,
            density,
            typography,
            |this, window, cx| this.submit_schedule_draft(window, cx),
            cx,
        )),
    )
    .into_any_element()
}

/// A stacked label + text input row for one create-form field.
fn text_field(
    label: &'static str,
    input: &Option<Entity<InputState>>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let control = match input {
        Some(state) => Input::new(state)
            .small()
            .text_size(px(typography.t_body_sm))
            .into_any_element(),
        None => div().into_any_element(),
    };
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(4.0))
        .py(px(6.0))
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(label),
        )
        .child(control)
        .into_any_element()
}

/// The Every-N / Daily / Weekly selector.
fn recurrence_kind_picker(
    kind: DraftKind,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    segmented(
        "sched-kind",
        vec![
            Segment::new("Every N min", kind == DraftKind::Interval, |this, _w, cx| {
                this.schedule_draft.kind = DraftKind::Interval;
                cx.notify();
            }),
            Segment::new("Daily", kind == DraftKind::Daily, |this, _w, cx| {
                this.schedule_draft.kind = DraftKind::Daily;
                cx.notify();
            }),
            Segment::new("Weekly", kind == DraftKind::Weekly, |this, _w, cx| {
                this.schedule_draft.kind = DraftKind::Weekly;
                cx.notify();
            }),
        ],
        theme,
        density,
        typography,
        cx,
    )
}

/// The value editors for whichever recurrence kind is selected: an interval
/// stepper, a time-of-day pair, or a weekday picker plus a time-of-day pair.
fn recurrence_editor(
    draft: ScheduleDraft,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    match draft.kind {
        DraftKind::Interval => setting_row_desc(
            "Interval",
            "At least five minutes — each fire starts a full agent turn.",
            stepper(
                "sched-interval",
                format!("{} min", draft.interval_minutes),
                theme,
                density,
                typography,
                |this, _w, cx| {
                    let m = this.schedule_draft.interval_minutes;
                    this.schedule_draft.interval_minutes =
                        m.saturating_sub(MINUTE_STEP).max(MIN_INTERVAL_MINUTES);
                    cx.notify();
                },
                |this, _w, cx| {
                    this.schedule_draft.interval_minutes += MINUTE_STEP;
                    cx.notify();
                },
                cx,
            ),
            theme,
            typography,
        ),
        DraftKind::Daily => setting_row_desc(
            "Time of day",
            "Local time.",
            time_of_day(draft, theme, density, typography, cx),
            theme,
            typography,
        ),
        DraftKind::Weekly => div()
            .flex()
            .flex_col()
            .w_full()
            .child(setting_row_desc(
                "Weekday",
                "",
                weekday_picker(draft.weekday, theme, density, typography, cx),
                theme,
                typography,
            ))
            .child(setting_row_desc(
                "Time of day",
                "Local time.",
                time_of_day(draft, theme, density, typography, cx),
                theme,
                typography,
            ))
            .into_any_element(),
    }
}

/// An `HH : MM` pair of steppers. Hour wraps 0–23; minute steps by five and
/// wraps 0–55.
fn time_of_day(
    draft: ScheduleDraft,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let hour = stepper(
        "sched-hour",
        format!("{:02}", draft.hour),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.schedule_draft.hour = (this.schedule_draft.hour + 23) % 24;
            cx.notify();
        },
        |this, _w, cx| {
            this.schedule_draft.hour = (this.schedule_draft.hour + 1) % 24;
            cx.notify();
        },
        cx,
    );
    let minute = stepper(
        "sched-minute",
        format!("{:02}", draft.minute),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.schedule_draft.minute = (this.schedule_draft.minute + 60 - MINUTE_STEP as u8) % 60;
            cx.notify();
        },
        |this, _w, cx| {
            this.schedule_draft.minute = (this.schedule_draft.minute + MINUTE_STEP as u8) % 60;
            cx.notify();
        },
        cx,
    );
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(hour)
        .child(div().text_color(theme.fg_muted).child(":"))
        .child(minute)
        .into_any_element()
}

/// A seven-segment weekday selector, Monday-first.
fn weekday_picker(
    selected: u8,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let segments = WEEKDAYS
        .iter()
        .enumerate()
        .map(|(idx, label)| {
            let day = idx as u8;
            Segment::new(*label, selected == day, move |this, _w, cx| {
                this.schedule_draft.weekday = day;
                cx.notify();
            })
        })
        .collect();
    segmented("sched-weekday", segments, theme, density, typography, cx)
}

fn schedule_list(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(6.0))
        .child(section_title("Schedules", "", theme, typography));

    if modal.schedule_rows.is_empty() {
        return col
            .child(
                div()
                    .py(px(8.0))
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child("No schedules yet. Create one above."),
            )
            .into_any_element();
    }

    for (idx, row) in modal.schedule_rows.iter().enumerate() {
        col = col.child(schedule_row(idx, row, theme, density, typography, cx));
    }
    col.into_any_element()
}

fn schedule_row(
    idx: usize,
    row: &ScheduleRow,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let s = &row.schedule;
    let enabled = s.enabled;
    let toggle_id = s.id.clone();
    let delete_id = s.id.clone();
    let subtitle = format!("{} · {}", describe(&s.recurrence), next_fire_label(s.next_fire_at));

    let mut card = div()
        .flex()
        .flex_col()
        .w_full()
        .py(px(10.0))
        .gap(px(6.0))
        .when(idx != 0, |c| c.border_t_1().border_color(theme.border_inactive))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(density.gap_inline))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(typography.t_body_sm))
                                .text_color(if enabled { theme.fg_base } else { theme.fg_muted })
                                .child(s.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(typography.t_sub_label))
                                .text_color(theme.fg_subtle)
                                .child(subtitle),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(density.gap_inline))
                        .child(toggle_switch(
                            ("sched-enabled", idx),
                            enabled,
                            theme,
                            move |this, _w, cx| {
                                this.toggle_schedule(toggle_id.clone(), !enabled, cx);
                            },
                            cx,
                        ))
                        .child(value_chip(
                            ("sched-delete", idx),
                            "Delete",
                            theme,
                            density,
                            typography,
                            move |this, _w, cx| this.delete_schedule(delete_id.clone(), cx),
                            cx,
                        )),
                ),
        );

    if !row.recent_runs.is_empty() {
        let mut runs = div().flex().flex_col().gap(px(2.0)).pl(px(2.0));
        for run in &row.recent_runs {
            runs = runs.child(run_line(run, theme, typography));
        }
        card = card.child(runs);
    }

    card.into_any_element()
}

/// One run-history line: a status dot, when it fired, and — on failure — why.
fn run_line(run: &ScheduleRun, theme: Theme, typography: &Typography) -> AnyElement {
    let (dot, text) = run_summary(run, theme);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(div().size(px(6.0)).flex_none().rounded_full().bg(dot))
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(text),
        )
        .into_any_element()
}

/// Dot colour + text for a run. Pure so the wording is unit-tested.
fn run_summary(run: &ScheduleRun, theme: Theme) -> (Hsla, String) {
    let when = run.fired_at.format("%b %-d %H:%M");
    match run.outcome {
        RunOutcome::Ok => (theme.status_ok, format!("{when} · ran")),
        RunOutcome::Failed => {
            let why = run.detail.as_deref().unwrap_or("failed");
            (theme.status_error, format!("{when} · failed — {why}"))
        }
    }
}

/// "next Jul 23 at 09:00" — the schedule's armed next-fire, for a person.
fn next_fire_label(next: DateTime<Local>) -> String {
    format!("next {}", next.format("%b %-d at %H:%M"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daily_draft_builds_a_daily_recurrence() {
        let draft = ScheduleDraft { kind: DraftKind::Daily, hour: 9, minute: 30, ..Default::default() };
        assert_eq!(draft.to_recurrence(), Ok(Recurrence::DailyAt { hour: 9, minute: 30 }));
    }

    #[test]
    fn a_weekly_draft_carries_its_weekday() {
        let draft =
            ScheduleDraft { kind: DraftKind::Weekly, weekday: 4, hour: 17, minute: 0, ..Default::default() };
        assert_eq!(
            draft.to_recurrence(),
            Ok(Recurrence::WeeklyAt { weekday: 4, hour: 17, minute: 0 })
        );
    }

    /// The interval floor is enforced at construction, so a draft under it fails
    /// to build rather than creating a runaway schedule.
    #[test]
    fn an_interval_draft_under_the_floor_is_refused() {
        let draft = ScheduleDraft { kind: DraftKind::Interval, interval_minutes: 1, ..Default::default() };
        assert!(draft.to_recurrence().is_err());
    }

    #[test]
    fn a_failed_run_summary_carries_its_detail() {
        let theme = Theme::default();
        let run = ScheduleRun {
            schedule_id: "s".into(),
            fired_at: Local::now(),
            outcome: RunOutcome::Failed,
            session_id: None,
            detail: Some("that working directory is not usable".into()),
        };
        let (_dot, text) = run_summary(&run, theme);
        assert!(text.contains("failed"), "names the failure: {text}");
        assert!(text.contains("not usable"), "surfaces the detail: {text}");
    }

    #[test]
    fn an_ok_run_summary_reads_as_ran() {
        let theme = Theme::default();
        let run = ScheduleRun {
            schedule_id: "s".into(),
            fired_at: Local::now(),
            outcome: RunOutcome::Ok,
            session_id: Some("agent-1".into()),
            detail: None,
        };
        let (_dot, text) = run_summary(&run, theme);
        assert!(text.contains("ran"), "reads as ran: {text}");
    }
}
