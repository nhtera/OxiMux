//! Settings › Agents › Session search: the opt-in switch for the local
//! full-text index of past conversations, its live status, and Clear.
//!
//! Status is never read from SQLite on the UI thread: while the indexer runs
//! its counters are a mutex read; while it is off, a background task reads the
//! file's numbers. Either way the card renders the last value it has and a
//! poll loop (only while this pane is showing) refreshes it.

use std::time::Duration;

use gpui::{AnyElement, BorrowAppContext, IntoElement, ParentElement, Styled, div, px};
use oximux_agents::session_search::IndexStatus;
use oximux_settings::{Density, SessionSearchSettings, Theme, Typography};

use super::SettingsModal;
use super::controls::{ChipTone, action_chip, toggle_switch};
use super::layout::{SettingEntry, card_surface, entries_card, entry, section_title};
use crate::session_search_service::SessionSearchService;
use crate::shell::settings_modal::SettingsPane;

/// Refresh cadence while a pass is reading files, and while settled.
const POLL_BUSY: Duration = Duration::from_secs(2);
const POLL_IDLE: Duration = Duration::from_secs(10);

/// Per-modal UI state for the card.
#[derive(Default)]
pub(crate) struct SessionSearchPane {
    poll_running: bool,
    /// Last status read for an index that is not running.
    idle: Option<IndexStatus>,
    /// First Clear click arms it; the second clears.
    confirm_clear: bool,
    clearing: bool,
}

fn enabled(cx: &gpui::App) -> bool {
    cx.try_global::<SessionSearchSettings>().is_some_and(|s| s.enabled)
}

/// What the card shows right now.
fn current_status(modal: &SettingsModal, cx: &gpui::App) -> Option<IndexStatus> {
    cx.try_global::<SessionSearchService>()
        .and_then(SessionSearchService::live_status)
        .or_else(|| modal.session_search.idle.clone())
}

pub(super) fn render_section(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    if !modal.session_search.poll_running {
        // Not from inside render: start it once this frame is done.
        let this = cx.entity();
        cx.defer(move |cx| this.update(cx, |m, cx| m.ensure_session_search_poll(cx)));
    }
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(8.0))
        .child(section_title(
            "Session search",
            "Search the full text of past agent conversations and tool output. The index stays on this computer.",
            theme,
            typography,
        ))
        .child(card_surface(
            theme,
            density,
            entries_card(theme, density, typography, entries(modal, theme, density, typography, cx)),
        ))
        .into_any_element()
}

pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let on = enabled(cx);
    let toggle = toggle_switch(
        "session-search-enabled",
        on,
        theme,
        |this: &mut SettingsModal, _w, cx| {
            // A Clear holds toggles until it has deleted the index.
            if this.session_search.clearing {
                return;
            }
            let next = SessionSearchSettings { enabled: !enabled(cx) };
            if let Err(err) = crate::session_search_settings::save(&next, cx) {
                tracing::warn!(%err, "session_search.toml write failed");
            }
            this.session_search.idle = None;
            // Turning off: read the file's numbers now (the indexer is told to
            // stop only after this handler returns).
            if !next.enabled {
                this.refresh_session_search_now(cx);
            }
            this.ensure_session_search_poll(cx);
            cx.notify();
        },
        cx,
    );
    let status = current_status(modal, cx);
    let pane = &modal.session_search;
    let has_data = status.as_ref().is_some_and(|s| s.db_bytes > 0);
    let clear_label = match (pane.clearing, pane.confirm_clear) {
        (true, _) => "Clearing…",
        (false, true) => "Click again to clear",
        (false, false) => "Clear search data",
    };
    let clear = action_chip(
        "session-search-clear",
        clear_label,
        ChipTone::Danger,
        has_data && !pane.clearing,
        theme,
        density,
        typography,
        |this: &mut SettingsModal, _w, cx| {
            if this.session_search.confirm_clear {
                this.clear_session_search(cx);
            } else {
                this.session_search.confirm_clear = true;
            }
            cx.notify();
        },
        cx,
    );
    let size = status.as_ref().map_or_else(|| "—".to_string(), |s| format_bytes(s.db_bytes));
    vec![
        entry(
            "Search inside conversations",
            "Index Claude and Codex transcripts so Session History (⌘⇧H) can search what was said, not just titles.",
            toggle,
        ),
        entry("Index", status_line(on, status.as_ref()), status_value(&size, theme, typography)),
        entry("Search data", "Deleting it is safe: the index is rebuilt from the transcripts.", clear),
    ]
}

fn status_value(text: &str, theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_muted)
        .child(text.to_string())
        .into_any_element()
}

/// The Index row's description: progress, totals, or why there are none.
pub(super) fn status_line(enabled: bool, status: Option<&IndexStatus>) -> String {
    match status {
        Some(s) if s.error.is_some() && enabled => format!("Index error: {}", s.error.as_deref().unwrap_or_default()),
        Some(s) if s.indexing && s.files_total > 0 => {
            format!("Indexing… {} of {} files", s.files_done, s.files_total)
        }
        Some(s) if s.sessions > 0 || s.messages > 0 => {
            let totals =
                format!("{} sessions · {} messages searchable", compact(s.sessions), compact(s.messages));
            if enabled { totals } else { format!("Off · {totals}") }
        }
        _ if enabled => "Starting…".to_string(),
        _ => "Off".to_string(),
    }
}

/// `764600` → `764.6K`, `1200000` → `1.2M`.
pub(super) fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => trim_zero(format!("{:.1}K", n as f64 / 1_000.0)),
        _ => trim_zero(format!("{:.1}M", n as f64 / 1_000_000.0)),
    }
}

fn trim_zero(s: String) -> String {
    s.replace(".0K", "K").replace(".0M", "M")
}

pub(super) fn format_bytes(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    match b {
        0 => "None".to_string(),
        1..1_048_576 => format!("{:.0} KB", (b as f64 / 1024.0).ceil()),
        _ if (b as f64) < 1024.0 * MB => format!("{:.0} MB", b as f64 / MB),
        _ => format!("{:.1} GB", b as f64 / (1024.0 * MB)),
    }
}

/// The index file to read numbers from while no indexer is running.
fn idle_db_path(cx: &gpui::App) -> Option<std::path::PathBuf> {
    let svc = cx.try_global::<SessionSearchService>()?;
    (!svc.is_running()).then(|| svc.db_path().map(|p| p.to_path_buf())).flatten()
}

impl SettingsModal {
    /// One immediate status read (the poll loop may be mid-sleep), so the
    /// card reflects a toggle at once.
    fn refresh_session_search_now(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(path) = cx
            .try_global::<SessionSearchService>()
            .and_then(|s| s.db_path().map(|p| p.to_path_buf()))
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { oximux_agents::session_search::read_status(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.session_search.idle = Some(status);
                cx.notify();
            });
        })
        .detach();
    }

    /// Keep the card's numbers fresh while it is showing; ends itself when
    /// the modal closes or another pane is selected.
    pub(super) fn ensure_session_search_poll(&mut self, cx: &mut gpui::Context<Self>) {
        if self.session_search.poll_running {
            return;
        }
        self.session_search.poll_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                // While off, read the file's numbers on a background thread.
                let idle_path = this.read_with(cx, |_, cx| idle_db_path(cx)).ok().flatten();
                let idle = match idle_path {
                    Some(p) => Some(
                        cx.background_executor()
                            .spawn(async move { oximux_agents::session_search::read_status(&p) })
                            .await,
                    ),
                    None => None,
                };
                let busy = this.update(cx, |this, cx| {
                    if !this.open || this.selected != SettingsPane::Agents {
                        this.session_search.poll_running = false;
                        return None;
                    }
                    this.session_search.idle = idle;
                    cx.notify();
                    Some(current_status(this, cx).is_some_and(|s| s.indexing))
                });
                let Ok(Some(busy)) = busy else { break };
                cx.background_executor().timer(if busy { POLL_BUSY } else { POLL_IDLE }).await;
            }
        })
        .detach();
    }

    /// Stop the indexer, delete the index off the UI thread, restart if on.
    fn clear_session_search(&mut self, cx: &mut gpui::Context<Self>) {
        if !cx.has_global::<SessionSearchService>() {
            return;
        }
        self.session_search.confirm_clear = false;
        self.session_search.clearing = true;
        let job = cx.update_global::<SessionSearchService, _>(|s, _| s.begin_clear());
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { job.run() }).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = result {
                    tracing::warn!(%err, "clearing the session index failed");
                }
                let on = enabled(cx);
                cx.update_global::<SessionSearchService, _>(|s, _| s.resume(on));
                this.session_search.clearing = false;
                this.session_search.idle = None;
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_counts() {
        assert_eq!(compact(842), "842");
        assert_eq!(compact(764_600), "764.6K");
        assert_eq!(compact(2_000), "2K");
        assert_eq!(compact(1_250_000), "1.2M");
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(format_bytes(0), "None");
        assert_eq!(format_bytes(1500), "2 KB");
        assert_eq!(format_bytes(412 * 1024 * 1024), "412 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
    }

    #[test]
    fn status_lines() {
        let s = IndexStatus { sessions: 842, messages: 294_628, ..Default::default() };
        assert_eq!(status_line(true, Some(&s)), "842 sessions · 294.6K messages searchable");
        assert_eq!(status_line(false, Some(&s)), "Off · 842 sessions · 294.6K messages searchable");
        let busy = IndexStatus { indexing: true, files_done: 3, files_total: 90, ..s.clone() };
        assert_eq!(status_line(true, Some(&busy)), "Indexing… 3 of 90 files");
        assert_eq!(status_line(true, None), "Starting…");
        assert_eq!(status_line(false, None), "Off");
    }
}
