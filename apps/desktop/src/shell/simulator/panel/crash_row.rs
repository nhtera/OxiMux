//! The crash row: an app on the shown device crashed (see the hub's
//! `crash_watch`). The user may hand it to the agent — a short summary,
//! secrets scrubbed, and the screen as the row came up — or dismiss it.
//! Nothing is ever sent without that click, and a click sends at once, to
//! the chat active then (the screen is taken when the row appears, so
//! nothing is waited for after the click).

use std::time::Duration;

use gpui::{AnyElement, Context, Window};
use oximux_agent_core::scrub_text::scrub_text;
use oximux_simulator::DeviceId;
use oximux_simulator::diag::Crash;

use super::SimulatorPanel;
use crate::shell::simulator::agent_ops::clamp;
use crate::shell::simulator::annotate::export;

/// The most a summary takes in the agent's composer.
const MAX_SUMMARY: usize = 4096;

impl SimulatorPanel {
    /// A crash on `udid`: the newest replaces any shown before; the same
    /// crash again (its frames arrived) updates its row, and never brings
    /// back one dismissed or sent. Scrubbed once, here, for the row and
    /// whatever is sent.
    pub(super) fn crashed(&mut self, udid: &DeviceId, crash: &Crash, cx: &mut Context<Self>) {
        let showing = self.crash.as_ref().is_some_and(|(shown_on, shown)| shown_on == udid && shown.id == crash.id);
        // An id is a device's own (Android's is a pid and a device-local
        // time): keyed with the device.
        if self.device(cx).as_ref() == Some(udid) && (showing || self.crash_ids.insert((udid.clone(), crash.id.clone()))) {
            if !showing {
                self.take_crash_screen(udid, &crash.id, cx);
            }
            let crash = Crash {
                app: clamp(&scrub_text(&crash.app), 256),
                exception: clamp(&scrub_text(&crash.exception), 300),
                reason: clamp(&scrub_text(&crash.reason), 1500),
                frames: crash.frames.iter().map(|f| clamp(&scrub_text(f), 200)).collect(),
                ..crash.clone()
            };
            self.crash = Some((udid.clone(), crash));
            cx.notify();
        }
    }

    /// The row, for the shown device's last crash.
    pub(super) fn render_crash_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (udid, crash) = self.crash.as_ref()?;
        if self.device(cx).as_ref() != Some(udid) {
            return None;
        }
        let why = [crash.exception.as_str(), crash.reason.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(": ");
        let title = if crash.app.is_empty() { "An app crashed".to_owned() } else { format!("{} crashed", crash.app) };
        Some(self.render_question(
            title,
            why,
            ("sim-crash-dismiss", "Dismiss"),
            ("sim-crash-send", "Send to agent"),
            cx.listener(|this, _, _window, cx| {
                this.crash = None;
                this.crash_png = None;
                cx.notify();
            }),
            cx.listener(|this, _, window, cx| this.send_crash(window, cx)),
        ))
    }

    /// The screen as the row comes up, kept for Send (a bonus: the summary
    /// goes without it if it is not there yet).
    fn take_crash_screen(&mut self, udid: &DeviceId, id: &str, cx: &mut Context<Self>) {
        self.crash_png = None;
        let Some(session) = self.hub.as_ref().and_then(|hub| hub.read(cx).session(udid)) else { return };
        let key = (udid.clone(), id.to_owned());
        cx.spawn(async move |this, cx| {
            let png = cx
                .background_executor()
                .spawn(async move {
                    let png = session.screenshot_png(Duration::from_secs(5)).ok()?;
                    export::fit_long_edge(&png, export::AGENT_LONG_EDGE).ok()
                })
                .await;
            let _ = this.update(cx, |panel, _| {
                // Still the row it was taken for: the same device and crash
                // (an Android id is only its device's own).
                if let Some(png) = png
                    && panel.crash.as_ref().is_some_and(|(udid, c)| *udid == key.0 && c.id == key.1)
                {
                    panel.crash_png = Some((key.0, key.1, png));
                }
            });
        })
        .detach();
    }

    /// Hand the crash to the agent now, to the chat active now.
    fn send_crash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((udid, crash)) = self.crash.take() else { return };
        cx.notify();
        let markdown = summary(&self.device_name(cx), &crash);
        let png = self.crash_png.take().filter(|(on, id, _)| *on == udid && *id == crash.id).map(|(_, _, png)| png).unwrap_or_default();
        self.send_to_agent(markdown, png, window, cx);
    }
}

/// What the agent reads: the app, the exception, the reason and the frames
/// (each already scrubbed and clamped by [`SimulatorPanel::crashed`], so the
/// whole stays under [`MAX_SUMMARY`] with its fence closed).
pub(super) fn summary(device: &str, crash: &Crash) -> String {
    let app = if crash.app.is_empty() { "An app" } else { crash.app.as_str() };
    let mut text = format!("{app} crashed on {} at {}.\n\n```\n{}", clamp(device, 100), clamp(&crash.time, 40), crash.exception);
    if !crash.reason.is_empty() {
        text.push_str(&format!("\n{}", crash.reason));
    }
    for (i, frame) in crash.frames.iter().take(oximux_simulator::diag::FRAMES).enumerate() {
        text.push_str(&format!("\n  {i} {frame}"));
    }
    text.push_str("\n```\nFind the cause and fix it (`oximux sim crashes` and `oximux sim logs` show more).");
    debug_assert!(text.len() <= MAX_SUMMARY, "{}", text.len());
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crash(reason: &str, frames: usize) -> Crash {
        Crash {
            id: "1".into(),
            time: "2026-10-12 15:54:29".into(),
            app: "dev.oximux.crashy".into(),
            exception: "EXC_BREAKPOINT (SIGTRAP)".into(),
            reason: reason.into(),
            frames: (0..frames).map(|i| format!("Crashy frame {i}")).collect(),
            pid: Some(1),
        }
    }

    /// As the row keeps it: scrubbed and clamped on arrival.
    fn kept(crash: Crash) -> Crash {
        Crash {
            exception: clamp(&scrub_text(&crash.exception), 300),
            reason: clamp(&scrub_text(&crash.reason), 1500),
            frames: crash.frames.iter().map(|f| clamp(&scrub_text(f), 200)).collect(),
            ..crash
        }
    }

    #[test]
    fn the_summary_names_what_why_and_where_scrubbed() {
        let text = summary("iPhone 17", &kept(crash("Fatal error: login failed token=abc123secret", 2)));
        assert!(text.starts_with("dev.oximux.crashy crashed on iPhone 17 at 2026-10-12 15:54:29.\n\n```\nEXC_BREAKPOINT (SIGTRAP)\n"));
        assert!(text.contains("token=[redacted]") && !text.contains("abc123secret"));
        assert!(text.contains("\n  1 Crashy frame 1\n```"));
    }

    #[test]
    fn a_long_summary_stays_under_four_kib_with_its_fence_closed() {
        let mut long = crash(&"é".repeat(5000), 8);
        long.frames = (0..8).map(|_| "f".repeat(1000)).collect();
        let text = summary(&"d".repeat(500), &kept(long));
        assert!(text.len() <= MAX_SUMMARY, "{}", text.len());
        assert!(text.contains("…\n```\nFind the cause"), "the fence closes");
        assert_eq!(clamp("abc", 5), "abc");
        assert_eq!(clamp("ééé", 3), "é…");
    }
}
