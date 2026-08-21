//! Automations page — rendered when the Automations pane tab is active.
//!
//! A first-class view of the same [`ScheduleStore`] the ticker fires and the
//! Schedules settings pane edits. Nothing new is stored here: the subsystem
//! (recurrence arithmetic, the durable claim, run recording) already shipped;
//! what was missing was a surface with room to show it.
//!
//! **Why a page and not just the settings pane.** A schedule is a thing that
//! runs while you are not watching, so the questions it has to answer are
//! "will this fire?", "when?", and "what happened last time?". The settings
//! row has ~250px and answers the first two in one elided line. A pane has the
//! width to also show the working directory and the prompt, which is what
//! makes a list of four automations readable as four *different* jobs rather
//! than four names. Creation stays in Settings → Schedules — it is a six-field
//! form with a folder picker, and a second copy of it would be a second thing
//! to keep correct, not a second thing to use.
//!
//! **The banner is load-bearing.** A scheduled run fires only while an OxiMux
//! host is running; nothing here wakes a sleeping machine or relaunches a quit
//! app. A page that lists armed automations without saying so has told the
//! user something false, so the banner leads — same rule the settings pane
//! follows, same words.
//!
//! Layout (top → bottom): header (census + actions) → constraint banner →
//! scrolling card list, or an empty state in place of the list.

pub(crate) mod labels;

use chrono::Local;
use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, StatefulInteractiveElement as _, Styled, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px,
};
use oximux_agents::schedule::{Schedule, ScheduleRun, ScheduleStore, describe};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::settings_modal::controls::{toggle_switch, value_chip};
use crate::workspace_root::WorkspaceRoot;

use labels::{armed_summary, home_abbrev, next_fire_label, prompt_preview, run_summary};

/// How many recent runs each card shows. Matches the settings pane so the two
/// surfaces agree about how much history is "recent".
const RUNS_SHOWN: u32 = 3;

/// One schedule plus the tail of its run history, loaded together so the page
/// never reads SQLite mid-paint.
pub(crate) struct AutomationRow {
    pub schedule: Schedule,
    pub recent_runs: Vec<ScheduleRun>,
}

/// Delete is destructive and un-undoable, and the chip sits a few pixels from
/// the enable switch. Rather than a modal, the chip arms on the first click and
/// commits on the second — cheap to back out of (click anything else) and
/// impossible to trip by accident.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum DeleteArm {
    #[default]
    Idle,
    Armed(String),
}

impl DeleteArm {
    /// What a click on `id`'s delete chip should do next, given the current
    /// arm state. Pure so the two-step is unit-tested rather than clicked.
    pub(crate) fn click(&self, id: &str) -> DeleteAction {
        match self {
            DeleteArm::Armed(armed) if armed == id => DeleteAction::Delete,
            // Arming a different row disarms the first: only ever one chip
            // reads "Confirm?", so the armed row is never ambiguous.
            _ => DeleteAction::Arm,
        }
    }

    pub(crate) fn is_armed(&self, id: &str) -> bool {
        matches!(self, DeleteArm::Armed(a) if a == id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeleteAction {
    Arm,
    Delete,
}

pub struct AutomationsView {
    weak_root: WeakEntity<WorkspaceRoot>,
    /// Anchors pane focus. The page has no text input of its own — creation
    /// lives in Settings — so this handle is the whole focus story.
    focus_handle: FocusHandle,
    store: ScheduleStore,
    theme: Theme,
    density: Density,
    typography: Typography,
    rows: Vec<AutomationRow>,
    delete_arm: DeleteArm,
    /// Set when a store read or write failed, shown in place of the census.
    /// A page that silently rendered an empty list after a failed read would
    /// claim "no automations" about a database it could not open.
    error: Option<String>,
    /// Home directory, resolved once per load for path abbreviation.
    home: Option<String>,
    scroll: ScrollHandle,
}

impl AutomationsView {
    pub fn new(
        weak_root: WeakEntity<WorkspaceRoot>,
        store: ScheduleStore,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            weak_root,
            focus_handle: cx.focus_handle(),
            store,
            theme,
            density,
            typography,
            rows: Vec::new(),
            delete_arm: DeleteArm::Idle,
            error: None,
            home: dirs::home_dir().map(|p| p.to_string_lossy().to_string()),
            scroll: ScrollHandle::new(),
        }
    }

    /// Re-read the store. Called on open, on re-activation, and after every
    /// mutation, so the page reflects what the ticker is also reading.
    ///
    /// `list_spawning`, not `list`: heartbeats share this table but are an
    /// agent's own wake-ups inside one conversation — arming, pausing, or
    /// deleting one is meaningless or harmful. Same narrowing the settings
    /// pane and the remote dispatcher apply.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        match self.store.list_spawning() {
            Ok(schedules) => {
                self.rows = schedules
                    .into_iter()
                    .map(|schedule| {
                        let recent_runs =
                            self.store.runs(&schedule.id, RUNS_SHOWN).unwrap_or_default();
                        AutomationRow { schedule, recent_runs }
                    })
                    .collect();
                self.error = None;
            }
            Err(err) => {
                tracing::warn!(%err, "automations: could not list schedules");
                self.rows.clear();
                self.error = Some("Could not read automations from the database.".into());
            }
        }
        // An armed delete does not survive a reload: the row it pointed at may
        // no longer exist, and a stale "Confirm?" is exactly the state that
        // makes a two-step feel unsafe.
        self.delete_arm = DeleteArm::Idle;
        cx.notify();
    }

    /// Called when the page becomes visible (open or re-activate). Always
    /// re-reads: the ticker mutates this store from a background task, so a
    /// cached list is stale the moment a run fires.
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    fn toggle(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        if let Err(err) = self.store.set_enabled(&id, enabled, Local::now()) {
            tracing::warn!(%err, "automations: could not toggle schedule");
            self.error = Some("Could not change that automation.".into());
        }
        self.reload(cx);
    }

    fn on_delete_click(&mut self, id: String, cx: &mut Context<Self>) {
        match self.delete_arm.click(&id) {
            DeleteAction::Arm => {
                self.delete_arm = DeleteArm::Armed(id);
                cx.notify();
            }
            DeleteAction::Delete => {
                if let Err(err) = self.store.delete(&id) {
                    tracing::warn!(%err, "automations: could not delete schedule");
                    self.error = Some("Could not delete that automation.".into());
                }
                self.reload(cx);
            }
        }
    }

    /// Hand off to Settings → Schedules, which owns the create form.
    fn open_create_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.weak_root.update(cx, |root, cx| {
            root.open_schedule_settings(window, cx);
        });
    }

    fn enabled_count(&self) -> usize {
        self.rows.iter().filter(|r| r.schedule.enabled).count()
    }
}

impl Focusable for AutomationsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AutomationsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        let body: AnyElement = if self.rows.is_empty() {
            empty_state(self.error.as_deref(), theme, density, &typography)
        } else {
            let mut col = div()
                .id("automations-list")
                .flex()
                .flex_col()
                .w_full()
                .flex_1()
                .min_h(px(0.))
                .gap(px(density.gap_inline))
                .p(px(density.pad_panel))
                .overflow_y_scroll()
                .track_scroll(&self.scroll);
            // Index-keyed element ids: the schedule id is a string, and the
            // chip/switch ids want a stable ordinal within one render.
            for idx in 0..self.rows.len() {
                col = col.child(self.render_card(idx, cx));
            }
            col.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(theme.bg_panel)
            .child(self.render_header(cx))
            .child(constraint_banner(theme, density, &typography))
            .child(body)
    }
}

impl AutomationsView {
    fn render_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let (census, census_color) = match &self.error {
            Some(msg) => (msg.clone(), theme.status_error),
            None => (armed_summary(self.rows.len(), self.enabled_count()), theme.fg_subtle),
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .flex_none()
            .h(px(density.h_top_bar))
            .px(px(density.pad_panel))
            .border_b_1()
            .border_color(theme.border_inactive)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(typography.t_body_md))
                            .font_weight(typography.w_semibold)
                            .text_color(theme.fg_base)
                            .child("Automations"),
                    )
                    .child(
                        div()
                            .text_size(px(typography.t_sub_label))
                            .text_color(census_color)
                            .child(census),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(density.gap_inline))
                    .child(value_chip(
                        "automations-refresh",
                        "Refresh",
                        theme,
                        density,
                        &typography,
                        |this: &mut Self, _w, cx| this.reload(cx),
                        cx,
                    ))
                    .child(value_chip(
                        "automations-new",
                        "New automation",
                        theme,
                        density,
                        &typography,
                        |this: &mut Self, window, cx| this.open_create_form(window, cx),
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_card(&mut self, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let row = &self.rows[idx];
        let s = &row.schedule;
        let enabled = s.enabled;
        let id = s.id.clone();
        let armed = self.delete_arm.is_armed(&id);
        let name = s.name.clone();
        let cadence = format!("{} · {}", describe(&s.recurrence), next_fire_label(s.next_fire_at));
        let cwd = home_abbrev(&s.cwd, self.home.as_deref());
        let prompt = prompt_preview(&s.prompt);
        let runs: Vec<AnyElement> = row
            .recent_runs
            .iter()
            .map(|run| run_line(run, theme, &typography))
            .collect();

        let toggle_id = id.clone();
        let delete_id = id;

        let mut card = div()
            .flex()
            .flex_col()
            .w_full()
            .flex_none()
            .gap(px(6.0))
            .p(px(density.pad_row))
            .rounded(px(density.r_card))
            .bg(theme.bg_panel_alt)
            .border_1()
            .border_color(theme.border_inactive)
            // A paused automation reads as inert rather than merely
            // unlabelled: the whole card steps back, not just the switch.
            .when(!enabled, |c| c.opacity(0.72))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .justify_between()
                    .gap(px(density.gap_inline))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            // `flex_1` AND `min_w_0`: the first claims the row's
                            // free space, the second lets long text shrink
                            // instead of forcing the row wider. With only
                            // `min_w_0` the column shrinks to zero against its
                            // `flex_none` sibling and the name wraps one
                            // character per line.
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(typography.t_body_sm))
                                    .font_weight(typography.w_medium)
                                    .text_color(if enabled {
                                        theme.fg_base
                                    } else {
                                        theme.fg_muted
                                    })
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(px(typography.t_sub_label))
                                    .text_color(theme.fg_subtle)
                                    .child(cadence),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .flex_none()
                            .gap(px(density.gap_inline))
                            .child(toggle_switch(
                                ("automation-enabled", idx),
                                enabled,
                                theme,
                                move |this: &mut Self, _w, cx| {
                                    this.toggle(toggle_id.clone(), !enabled, cx)
                                },
                                cx,
                            ))
                            .child(value_chip(
                                ("automation-delete", idx),
                                if armed { "Confirm?" } else { "Delete" },
                                theme,
                                density,
                                &typography,
                                move |this: &mut Self, _w, cx| {
                                    this.on_delete_click(delete_id.clone(), cx)
                                },
                                cx,
                            )),
                    ),
            )
            // What the settings row has no width for: where it runs and what
            // it asks. Without these, four automations read as four names.
            .child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_muted)
                    .child(cwd),
            )
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child(prompt),
            );

        if !runs.is_empty() {
            let mut history = div().flex().flex_col().gap(px(2.0)).pt(px(2.0));
            for line in runs {
                history = history.child(line);
            }
            card = card.child(history);
        }

        card.into_any_element()
    }
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

/// The standing constraint, stated before the list rather than under it.
fn constraint_banner(theme: Theme, density: Density, typography: &Typography) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .w_full()
        .px(px(density.pad_panel))
        .py(px(6.0))
        .bg(theme.bg_panel_alt)
        .border_b_1()
        .border_color(theme.border_inactive)
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_muted)
        .child(
            "Automations fire only while OxiMux is running. Quitting the app — or letting the \
             machine sleep — skips the runs that fall in the gap.",
        )
        .into_any_element()
}

fn empty_state(
    error: Option<&str>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    // A read failure is not an empty list: saying "No automations yet" over a
    // database we could not open would be a confident lie.
    let (title, subtitle) = match error {
        Some(_) => (
            "Automations are unavailable",
            "The schedule database could not be read. Reopening the app usually clears this.",
        ),
        None => (
            "No automations yet",
            "Create one in Settings → Schedules to have an agent run on a cadence.",
        ),
    };
    div()
        .flex()
        .flex_col()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(density.gap_inline))
        .p(px(density.pad_panel))
        .child(
            div()
                .text_size(px(typography.t_body_md))
                .text_color(theme.fg_muted)
                .child(title),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(subtitle),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_click_arms_rather_than_deleting() {
        assert_eq!(DeleteArm::Idle.click("sch-1"), DeleteAction::Arm);
    }

    #[test]
    fn a_second_click_on_the_armed_row_deletes() {
        let arm = DeleteArm::Armed("sch-1".into());
        assert_eq!(arm.click("sch-1"), DeleteAction::Delete);
    }

    /// The dangerous case: an armed row must not lend its confirmation to a
    /// different row's chip. Clicking a neighbour arms the neighbour instead.
    #[test]
    fn an_armed_row_does_not_confirm_its_neighbour() {
        let arm = DeleteArm::Armed("sch-1".into());
        assert_eq!(arm.click("sch-2"), DeleteAction::Arm);
    }

    #[test]
    fn only_the_armed_row_reads_as_armed() {
        let arm = DeleteArm::Armed("sch-1".into());
        assert!(arm.is_armed("sch-1"));
        assert!(!arm.is_armed("sch-2"));
        assert!(!DeleteArm::Idle.is_armed("sch-1"));
    }
}
