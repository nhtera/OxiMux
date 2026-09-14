//! Live provisioning progress — a floating card per in-flight worktree
//! create, fed by the same event stream the transcript file is written from.
//!
//! Provisioning (`.oximuxinclude` copy, default-branch freshen, the setup
//! script) already streams [`ProvisionEvent`]s; until now the desktop wrote
//! them to a file and showed nothing until the create ended. This layer
//! draws that stream as it happens. The model it paints — one create's
//! progress and the rules for revealing it — is `provision_progress.rs`.
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
//! A card's terminal state comes from the create task and only from it:
//! every path out of a create finishes its card, so there is no watchdog to
//! contradict a create that is merely slow.

use std::path::PathBuf;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, ElementId, Entity, IntoElement, ParentElement, Render,
    Styled, Window, div, px,
};
use gpui_component::Sizable;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::spinner::Spinner;
use oximux_settings::{Density, Theme, Typography};
use oximux_worktree_ops::ProvisionEvent;

use super::provision_progress::{ProvisionProgress, ProvisionState, SHOW_AFTER};
use crate::ui::FloatingSurface;

/// Lines the card paints.
const TAIL_LINES: usize = 8;
/// How long a successful card lingers before it goes.
const LINGER_AFTER_SUCCESS: Duration = Duration::from_secs(3);
/// Events the tee may hold before the card starts dropping them. The file
/// keeps everything; the card shows a tail, so a dropped line under a burst
/// costs nothing visible. Bounded so a script writing faster than the UI
/// thread drains cannot grow memory for the length of the run.
pub const TEE_CAPACITY: usize = 1024;

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
    /// finishes, and arms the reveal timer.
    pub fn begin(
        &mut self,
        slug: String,
        project_id: String,
        transcript: PathBuf,
        cx: &mut Context<Self>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(ProvisionProgress::new(id, slug, project_id, transcript));

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
        if !entry.finish(outcome) {
            return;
        }
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

    /// Ids of the cards currently painted.
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

        let header = div()
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
            )
            .child(
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
            let project_id = entry.project_id.clone();
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
                                        project_id: project_id.clone(),
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
            let entry = &self.entries[i];
            cards.push(self.render_card(entry, cx));
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
    layer: Entity<ProvisionLayer>,
    id: u64,
    mut rx: tokio::sync::mpsc::Receiver<ProvisionEvent>,
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
