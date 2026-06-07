//! Interactive CI-checks section for the Source Control panel.
//!
//! Promotes the compact one-line CI summary into a per-check list: each check
//! shows its status glyph + name + short blurb, a failing check expands to a
//! peek of its failed-job log, and a "Fix failing checks" affordance bundles
//! the failing logs into a markdown prompt for the active agent.
//!
//! The check *data* (`Vec<CheckRun>`) is owned by the panel and refreshed on
//! the same poll throttle as the PR status; this module holds only the
//! view-state layered on top (which check is expanded, fetched logs) plus the
//! pure render + prompt-formatting helpers.

use std::collections::HashMap;

use gpui::{
    AnyElement, InteractiveElement, IntoElement, MouseButton, ParentElement, StatefulInteractiveElement,
    Styled, WeakEntity, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use super::SourceControlPanel;
use super::ci_status::CheckSummary;
use crate::shell::forge::CheckRun;

/// A fetched (or in-flight) log peek for one check.
#[derive(Debug, Clone)]
pub enum LogPeek {
    /// Fetch is in flight.
    Loading,
    /// Log text (already tailed to a byte budget by the transport).
    Ready(String),
    /// No log available — non-Actions check, no failed jobs, or `gh` absent.
    Unavailable,
}

/// View-state for the checks section. Defaults to "nothing expanded".
#[derive(Debug, Default)]
pub struct ChecksSectionState {
    /// Name of the currently expanded check (one at a time), if any.
    pub expanded: Option<String>,
    /// Fetched log peeks keyed by check name.
    pub logs: HashMap<String, LogPeek>,
    /// True while a "fix failing checks" prompt is being assembled.
    pub fixing: bool,
}

impl ChecksSectionState {
    /// Drop expansion + cached logs — called when the underlying check set
    /// changes so a stale log can't linger under a now-different check.
    pub fn reset(&mut self) {
        self.expanded = None;
        self.logs.clear();
        self.fixing = false;
    }
}

/// Status glyph for a gh bucket, matching the compact CI row's vocabulary.
fn bucket_glyph(bucket: &str) -> &'static str {
    match bucket {
        "pass" => "✓",
        "fail" => "✗",
        "pending" => "●",
        _ => "·",
    }
}

fn bucket_color(bucket: &str, theme: Theme) -> gpui::Hsla {
    match bucket {
        "pass" => theme.status_ok,
        "fail" => theme.status_error,
        "pending" => theme.status_warning,
        _ => theme.fg_muted,
    }
}

/// Build the markdown prompt sent to the agent for "fix failing checks".
/// `sections` is `(check_name, optional_log)` for each failing check. Empty
/// string when there are no sections (the caller then dispatches nothing).
pub fn format_fix_prompt(sections: &[(String, Option<String>)]) -> String {
    if sections.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "The following CI checks are failing on this branch. Please investigate and fix them:\n",
    );
    for (name, log) in sections {
        out.push_str("\n## ");
        out.push_str(name);
        out.push('\n');
        match log {
            Some(text) if !text.trim().is_empty() => {
                out.push_str("\n```\n");
                out.push_str(text.trim_end());
                out.push_str("\n```\n");
            }
            _ => {
                out.push_str("\n_(no log available)_\n");
            }
        }
    }
    out
}

/// Render the interactive checks section, or `None` when there are no checks
/// worth showing (no real pass/fail/pending runs) — mirroring the compact
/// row's collapse rule so the panel reserves no dead space.
pub fn render_checks_section(
    checks: &[CheckRun],
    state: &ChecksSectionState,
    theme: Theme,
    density: Density,
    typography: &Typography,
    panel: WeakEntity<SourceControlPanel>,
) -> Option<AnyElement> {
    let summary = CheckSummary::from_runs(checks);
    if !summary.is_renderable() {
        return None;
    }

    let header = render_header(&summary, theme, density, panel.clone());

    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .px(px(density.pad_panel))
        .pb(px(density.gap_inline))
        .gap(px(density.gap_inline))
        .text_size(px(typography.t_sub_label))
        .child(header);

    // Per-check rows — skip the skipped/cancelled bucket (the summary already
    // excludes them from the renderable decision).
    for check in checks.iter().filter(|c| {
        matches!(c.bucket.as_str(), "pass" | "fail" | "pending")
    }) {
        col = col.child(render_check_row(check, state, theme, density, typography, panel.clone()));
    }

    Some(col.into_any_element())
}

fn render_header(
    summary: &CheckSummary,
    theme: Theme,
    density: Density,
    panel: WeakEntity<SourceControlPanel>,
) -> AnyElement {
    let headline_color = if summary.fail > 0 {
        theme.status_error
    } else if summary.pending > 0 {
        theme.status_warning
    } else {
        theme.status_ok
    };

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .text_color(headline_color)
                .child(summary.headline().to_string()),
        )
        .child(
            div()
                .text_color(theme.fg_subtle)
                .child(format!("({})", summary.total())),
        )
        .child(div().flex_1());

    // "Fix failing" chip — only when something actually failed.
    if summary.fail > 0 {
        let p = panel.clone();
        row = row.child(
            div()
                .id("checks-fix-failing")
                .px(px(6.0))
                .rounded(px(density.r_chip))
                .text_color(theme.status_error)
                .hover(|s| s.bg(theme.bg_panel_alt))
                .cursor_pointer()
                .child("Fix failing")
                .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                    let _ = p.update(cx, |panel, cx| panel.fix_failing_checks(window, cx));
                }),
        );
    }

    // Refresh chip — forces the next poll tick to re-check (bypasses throttle).
    let p = panel.clone();
    row = row.child(
        div()
            .id("checks-refresh")
            .px(px(6.0))
            .rounded(px(density.r_chip))
            .text_color(theme.fg_muted)
            .hover(|s| s.bg(theme.bg_panel_alt).text_color(theme.fg_base))
            .cursor_pointer()
            .child("Refresh")
            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                let _ = p.update(cx, |panel, cx| panel.refresh_checks(cx));
            }),
    );

    row.into_any_element()
}

fn render_check_row(
    check: &CheckRun,
    state: &ChecksSectionState,
    theme: Theme,
    density: Density,
    typography: &Typography,
    panel: WeakEntity<SourceControlPanel>,
) -> AnyElement {
    let is_failing = check.bucket == "fail";
    let is_expanded = state.expanded.as_deref() == Some(check.name.as_str());

    let mut content = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .text_color(bucket_color(&check.bucket, theme))
                .child(bucket_glyph(&check.bucket)),
        )
        .child(div().text_color(theme.fg_base).child(check.name.clone()));

    if !check.description.is_empty() {
        content = content.child(
            div()
                .text_color(theme.fg_muted)
                .child(check.description.clone()),
        );
    }

    // Only failing checks are interactive (the log peek uses --log-failed).
    let row_el: AnyElement = if is_failing {
        let name = check.name.clone();
        let link = check.link.clone();
        let p = panel.clone();
        content
            .id(gpui::SharedString::from(format!("check-{}", check.name)))
            .cursor_pointer()
            .hover(|s| s.text_color(theme.fg_base))
            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                let _ = p.update(cx, |panel, cx| {
                    panel.toggle_check(name.clone(), link.clone(), cx)
                });
            })
            .into_any_element()
    } else {
        content.into_any_element()
    };

    let mut entry = div().flex().flex_col().w_full().gap(px(2.0)).child(row_el);

    if is_failing && is_expanded {
        entry = entry.child(render_log_peek(state.logs.get(&check.name), theme, density, typography));
    }

    entry.into_any_element()
}

fn render_log_peek(
    peek: Option<&LogPeek>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let body: AnyElement = match peek {
        Some(LogPeek::Ready(text)) => {
            // Render one div per line so indentation + blank separators survive
            // exactly (no whitespace folding), the way the diff view paints its
            // monospace rows. Long lines don't wrap — the scroll box clips them.
            let mut col = div()
                .flex()
                .flex_col()
                .font_family("monospace")
                .text_size(px(typography.t_label_xs))
                .text_color(theme.fg_muted);
            for line in text.lines() {
                col = col.child(div().whitespace_nowrap().child(line.to_string()));
            }
            col.into_any_element()
        }
        Some(LogPeek::Loading) => div()
            .text_color(theme.fg_subtle)
            .child("Loading log…")
            .into_any_element(),
        _ => div()
            .text_color(theme.fg_subtle)
            .child("No log available")
            .into_any_element(),
    };
    div()
        .id("check-log-peek")
        .w_full()
        .max_h(px(220.0))
        .overflow_y_scroll()
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_base)
        .child(body)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_prompt_empty_for_no_sections() {
        assert_eq!(format_fix_prompt(&[]), "");
    }

    #[test]
    fn fix_prompt_includes_name_and_fenced_log() {
        let out = format_fix_prompt(&[("build".into(), Some("error: boom".into()))]);
        assert!(out.starts_with("The following CI checks are failing"));
        assert!(out.contains("## build"));
        assert!(out.contains("```"));
        assert!(out.contains("error: boom"));
    }

    #[test]
    fn fix_prompt_marks_missing_log() {
        let out = format_fix_prompt(&[("lint".into(), None)]);
        assert!(out.contains("## lint"));
        assert!(out.contains("no log available"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn fix_prompt_blank_log_treated_as_missing() {
        let out = format_fix_prompt(&[("test".into(), Some("   \n".into()))]);
        assert!(out.contains("no log available"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn state_reset_clears_everything() {
        let mut s = ChecksSectionState {
            expanded: Some("x".into()),
            fixing: true,
            ..Default::default()
        };
        s.logs.insert("x".into(), LogPeek::Loading);
        s.reset();
        assert!(s.expanded.is_none());
        assert!(s.logs.is_empty());
        assert!(!s.fixing);
    }
}
