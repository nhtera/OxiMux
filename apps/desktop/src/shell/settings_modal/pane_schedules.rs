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
//!
//! **Layout.** The create form sits in one grouped card; time-of-day is picked
//! with two `HH`/`MM` dropdowns rather than paired steppers, which read as a
//! stray `+ : −` expression. The working directory has a native folder picker so
//! a path cannot be fat-fingered.

use chrono::Local;
use gpui::{
    Anchor, AnyElement, ClickEvent, Context, Entity, IntoElement, ParentElement, SharedString,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, DropdownButton};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::PopupMenuItem;
use gpui_component::{Icon, Sizable as _};
use oximux_agents::schedule::{
    NewSchedule, Recurrence, RecurrenceError, Schedule, ScheduleRun, describe,
};
use oximux_settings::{Density, Theme, Typography};

// The wording every schedule surface shares. See that module for why these
// live beside the Automations page rather than here.
use crate::shell::automations_view::labels::{next_fire_label, run_summary};

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{card_surface, section_title, setting_row_desc};
use super::segmented::{Segment, segmented};

/// How many recent runs to show under each schedule.
const RUNS_SHOWN: u32 = 3;

/// Weekday labels, Monday-first to match [`Recurrence::WeeklyAt`]'s `0 = Monday`.
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Interval presets for the "Every N minutes" dropdown. All are at or above the
/// recurrence floor, so the picker cannot express a runaway schedule.
const INTERVAL_CHOICES: [u32; 8] = [5, 10, 15, 30, 60, 120, 240, 480];

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
    ///
    /// `list_spawning`, not `list`: heartbeats share this table but are an
    /// agent's own wake-ups inside one conversation, and every control this pane
    /// offers (edit the cwd, pick the agent, delete) is meaningless or harmful
    /// for a session that is already open. The remote surface narrows the same
    /// way — see `Dispatcher::list_schedules`.
    pub(super) fn reload_schedules(&mut self) {
        let schedules = self.schedule_store.list_spawning().unwrap_or_else(|err| {
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

    /// Open a native folder picker and drop the chosen path into the working-
    /// directory field. Mirrors the project picker's `pick_folder` flow: the
    /// panel resolves outside the GPUI window, so the result is applied back on
    /// the UI thread via `update_in`.
    pub(super) fn browse_schedule_cwd(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let folder = rfd::AsyncFileDialog::new().pick_folder().await;
            let Some(path) = folder.map(|h| h.path().to_path_buf()) else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(input) = this.sched_cwd_input.clone() {
                    input.update(cx, |s, cx| {
                        s.set_value(path.to_string_lossy().to_string(), window, cx)
                    });
                    cx.notify();
                }
            });
        })
        .detach();
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

fn set_hour(m: &mut SettingsModal, v: u32) {
    m.schedule_draft.hour = v as u8;
}
fn set_minute(m: &mut SettingsModal, v: u32) {
    m.schedule_draft.minute = v as u8;
}
fn set_interval(m: &mut SettingsModal, v: u32) {
    m.schedule_draft.interval_minutes = v;
}

pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let entity = cx.entity();
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(16.0))
        .child(constraint_banner(theme, typography))
        .child(section_title("New schedule", "", theme, typography))
        .child(card_surface(
            theme,
            density,
            create_form_body(modal, &entity, theme, density, typography, cx),
        ))
        .child(section_title("Your schedules", "", theme, typography))
        .child(schedule_list_body(modal, theme, density, typography, cx))
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

fn create_form_body(
    modal: &SettingsModal,
    entity: &Entity<SettingsModal>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let draft = modal.schedule_draft;
    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .py(px(4.0))
        .child(text_field("Name", &modal.sched_name_input, theme, typography))
        .child(cwd_field(&modal.sched_cwd_input, theme, density, typography, cx))
        .child(text_field("Prompt", &modal.sched_prompt_input, theme, typography))
        .child(setting_row_desc(
            "Repeats",
            "How often the run fires.",
            recurrence_kind_picker(draft.kind, theme, density, typography, cx),
            theme,
            typography,
        ))
        .child(recurrence_editor(draft, entity, theme, density, typography, cx));

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
        div().pt(px(10.0)).child(value_chip(
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
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(4.0))
        .py(px(6.0))
        .child(field_label(label, theme, typography))
        .child(input_or_blank(input, typography))
        .into_any_element()
}

/// The working-directory field: label, input, and a native folder picker so a
/// path can be chosen rather than typed.
fn cwd_field(
    input: &Option<Entity<InputState>>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(4.0))
        .py(px(6.0))
        .child(field_label("Working directory", theme, typography))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(div().flex_1().child(input_or_blank(input, typography)))
                .child(value_chip(
                    "sched-browse",
                    "Browse…",
                    theme,
                    density,
                    typography,
                    |this, _window, cx| this.browse_schedule_cwd(cx),
                    cx,
                )),
        )
        .into_any_element()
}

fn field_label(label: &'static str, theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_muted)
        .child(label)
        .into_any_element()
}

fn input_or_blank(input: &Option<Entity<InputState>>, typography: &Typography) -> AnyElement {
    match input {
        Some(state) => {
            Input::new(state).small().text_size(px(typography.t_body_sm)).into_any_element()
        }
        None => div().into_any_element(),
    }
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
/// dropdown, a time-of-day pair, or a weekday picker plus a time-of-day pair.
fn recurrence_editor(
    draft: ScheduleDraft,
    entity: &Entity<SettingsModal>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    match draft.kind {
        DraftKind::Interval => setting_row_desc(
            "Interval",
            "At least five minutes — each fire starts a full agent turn.",
            number_dropdown(
                "sched-interval",
                interval_label(draft.interval_minutes),
                interval_options(),
                draft.interval_minutes,
                entity.clone(),
                set_interval,
            ),
            theme,
            typography,
        ),
        DraftKind::Daily => setting_row_desc(
            "Time of day",
            "Local time.",
            time_of_day(draft, entity, theme),
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
                time_of_day(draft, entity, theme),
                theme,
                typography,
            ))
            .into_any_element(),
    }
}

/// An `HH : MM` pair of dropdowns — a clock, not two flanked steppers.
fn time_of_day(draft: ScheduleDraft, entity: &Entity<SettingsModal>, theme: Theme) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(number_dropdown(
            "sched-hour",
            format!("{:02}", draft.hour),
            hour_options(),
            draft.hour as u32,
            entity.clone(),
            set_hour,
        ))
        .child(div().px(px(2.0)).text_color(theme.fg_muted).child(":"))
        .child(number_dropdown(
            "sched-minute",
            format!("{:02}", draft.minute),
            minute_options(),
            draft.minute as u32,
            entity.clone(),
            set_minute,
        ))
        .into_any_element()
}

/// A labelled dropdown that writes a numeric draft field. The trigger shows the
/// current value; the menu lists `options`, the active one trailing a check.
fn number_dropdown(
    id: &'static str,
    current_label: String,
    options: Vec<(String, u32)>,
    selected_value: u32,
    entity: Entity<SettingsModal>,
    set: fn(&mut SettingsModal, u32),
) -> AnyElement {
    DropdownButton::new(id)
        .button(
            Button::new(SharedString::from(format!("{id}-btn")))
                .label(current_label)
                .small()
                .outline(),
        )
        .small()
        .dropdown_menu_with_anchor(Anchor::TopLeft, move |mut menu, window, _cx| {
            for (label, value) in options.clone() {
                let selected = value == selected_value;
                let entity = entity.clone();
                menu = menu.item(
                    PopupMenuItem::element(move |_w, _cx| menu_row(label.clone(), selected)).on_click(
                        window.listener_for(
                            &entity,
                            move |m: &mut SettingsModal, _ev: &ClickEvent, _w, cx| {
                                set(m, value);
                                cx.notify();
                            },
                        ),
                    ),
                );
            }
            menu
        })
        .into_any_element()
}

/// One dropdown menu row: the label, and a right-aligned check on the active one.
fn menu_row(label: String, selected: bool) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(28.0))
        .min_w(px(96.0))
        .child(div().child(label))
        .child(div().w(px(16.0)).flex_none().flex().justify_center().when(selected, |d| {
            d.child(Icon::default().path("icons/check.svg").size(px(14.0)))
        }))
}

fn hour_options() -> Vec<(String, u32)> {
    (0u32..24).map(|h| (format!("{h:02}"), h)).collect()
}

fn minute_options() -> Vec<(String, u32)> {
    (0u32..60).step_by(5).map(|m| (format!("{m:02}"), m)).collect()
}

fn interval_options() -> Vec<(String, u32)> {
    INTERVAL_CHOICES.iter().map(|&m| (interval_label(m), m)).collect()
}

/// A human interval, e.g. `30 min`, `1 hour`, `2 hours`.
fn interval_label(m: u32) -> String {
    if m >= 60 && m.is_multiple_of(60) {
        let hours = m / 60;
        if hours == 1 { "1 hour".into() } else { format!("{hours} hours") }
    } else {
        format!("{m} min")
    }
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

fn schedule_list_body(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    if modal.schedule_rows.is_empty() {
        return div()
            .py(px(4.0))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_subtle)
            .child("No schedules yet. Create one above.")
            .into_any_element();
    }

    let mut col = div().flex().flex_col().w_full().py(px(2.0));
    for (idx, row) in modal.schedule_rows.iter().enumerate() {
        col = col.child(schedule_row(idx, row, theme, density, typography, cx));
    }
    card_surface(theme, density, col.into_any_element())
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
    fn interval_labels_read_naturally() {
        assert_eq!(interval_label(30), "30 min");
        assert_eq!(interval_label(60), "1 hour");
        assert_eq!(interval_label(120), "2 hours");
    }

    /// Every interval preset is at or above the floor, so the picker cannot
    /// express a schedule the store would reject.
    #[test]
    fn interval_presets_all_clear_the_floor() {
        for m in INTERVAL_CHOICES {
            assert!(Recurrence::every_minutes(m).is_ok(), "{m} is below the floor");
        }
    }
}
