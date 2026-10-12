//! The crash row: an app on the shown device crashed (see the hub's
//! `crash_watch`). The user may hand it to the agent — a short summary,
//! secrets scrubbed, and the screen as it is now — or dismiss it. Nothing is
//! ever sent without that click.

use std::time::Duration;

use gpui::{AnyElement, Context, Window};
use oximux_agent_core::scrub_text::scrub_text;
use oximux_simulator::DeviceId;
use oximux_simulator::diag::Crash;

use super::SimulatorPanel;
use crate::shell::simulator::annotate::export;

/// The most a summary takes in the agent's composer.
const MAX_SUMMARY: usize = 4096;

impl SimulatorPanel {
    /// A crash on `udid`: the newest replaces any shown before.
    pub(super) fn crashed(&mut self, udid: &DeviceId, crash: &Crash, cx: &mut Context<Self>) {
        if self.device(cx).as_ref() == Some(udid) {
            self.crash = Some((udid.clone(), crash.clone()));
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
        Some(self.render_question(
            format!("{} crashed", crash.app),
            scrub_text(&why),
            ("sim-crash-dismiss", "Dismiss"),
            ("sim-crash-send", "Send to agent"),
            cx.listener(|this, _, _window, cx| {
                this.crash = None;
                cx.notify();
            }),
            cx.listener(|this, _, window, cx| this.send_crash(window, cx)),
        ))
    }

    /// Hand the crash to the agent, with the screen as it is now.
    fn send_crash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((udid, crash)) = self.crash.take() else { return };
        cx.notify();
        let markdown = summary(&self.device_name(cx), &crash);
        let session = self.hub.as_ref().and_then(|hub| hub.read(cx).session(&udid));
        cx.spawn_in(window, async move |this, cx| {
            // The screen is a bonus: the summary goes without it.
            let png = match session {
                Some(session) => cx
                    .background_executor()
                    .spawn(async move {
                        let png = session.screenshot_png(Duration::from_secs(5)).ok()?;
                        export::fit_long_edge(&png, export::AGENT_LONG_EDGE).ok()
                    })
                    .await
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            let _ = this.update_in(cx, |panel, window, cx| panel.send_to_agent(markdown, png, window, cx));
        })
        .detach();
    }
}

/// What the agent reads: the app, the exception, the reason and the frames,
/// scrubbed and at most [`MAX_SUMMARY`] bytes.
pub(super) fn summary(device: &str, crash: &Crash) -> String {
    let mut text = format!("{} crashed on {device} at {}.\n\n```\n{}", crash.app, crash.time, crash.exception);
    if !crash.reason.is_empty() {
        text.push_str(&format!("\n{}", crash.reason));
    }
    for (i, frame) in crash.frames.iter().enumerate() {
        text.push_str(&format!("\n  {i} {frame}"));
    }
    text.push_str("\n```\nFind the cause and fix it (`oximux sim crashes` and `oximux sim logs` show more).");
    let mut text = scrub_text(&text);
    if text.len() > MAX_SUMMARY {
        let cut = (0..=MAX_SUMMARY - 4).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n…");
    }
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

    #[test]
    fn the_summary_names_what_why_and_where_scrubbed() {
        let text = summary("iPhone 17", &crash("Fatal error: login failed token=abc123secret", 2));
        assert!(text.starts_with("dev.oximux.crashy crashed on iPhone 17 at 2026-10-12 15:54:29.\n\n```\nEXC_BREAKPOINT (SIGTRAP)\n"));
        assert!(text.contains("token=[redacted]") && !text.contains("abc123secret"));
        assert!(text.contains("\n  1 Crashy frame 1\n```"));
    }

    #[test]
    fn a_long_summary_is_cut_to_four_kib() {
        let text = summary("iPhone 17", &crash(&"é".repeat(5000), 8));
        assert!(text.len() <= MAX_SUMMARY, "{}", text.len());
        assert!(text.ends_with('…'));
    }
}
