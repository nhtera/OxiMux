//! Commit-graph row renderer + ref-chip cluster + hover tooltip body.
//!
//! Lives outside `graph.rs` so the `CommitGraph` entity module stays
//! under the 800-LOC hard cap as the v2 polish pass adds the
//! author-column auto-hide gate, the per-commit numstat stat line in
//! the hover tooltip, and the right-click affordance for copying the
//! commit's short OID.
//!
//! The row builder stays a free function (rather than a method on
//! `CommitGraph`) because the `uniform_list` factory in `graph.rs`
//! borrows the entity through a closure and only passes per-row data
//! by value — keeping the renderer pure keeps the closure signature
//! the simplest possible.

use gpui::{
    AnyElement, ClickEvent, ElementId, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement as _, Styled,
    WeakEntity, div, prelude::FluentBuilder as _, px,
};
use gpui_component::tooltip::Tooltip;
use oximux_core::{CommitInfo, RefLabel};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::source_control::graph::{CommitGraph, ShowCommitRequested};
use crate::shell::source_control::style as sc_style;

/// Max ref chips rendered inline per commit row before the overflow
/// `+N` chip kicks in. Tuned for the standard sidebar width — two
/// chips leave room for the subject, author, date, and SHA columns.
const REF_CHIPS_VISIBLE: usize = 2;

/// Render one commit row. `show_author` lets the graph entity hide the
/// author column when every visible commit belongs to the same person
/// (solo-author repo case — column would just be 20 copies of the same
/// name). `stats` is `Some((added, removed))` once the per-commit
/// numstat backend has populated the cache; until then the tooltip
/// renders without the stats line.
pub(super) fn render_commit_row(
    c: &CommitInfo,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<CommitGraph>,
    show_author: bool,
    stats: Option<(u32, u32)>,
) -> AnyElement {
    // Single-line row: dot + subject (truncates) + author + date + short sha.
    // Reference layout collapses the v1 two-line "subject / author • date"
    // into one tight row so 20+ commits stay scannable inside the sidebar.
    //
    // The timeline column stacks a connector line above the dot and another
    // below it, so consecutive rows draw an unbroken vertical line through
    // the dot centers (the canonical commit-graph spine).
    let timeline = div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(14.0))
        .h_full()
        .child(div().w(px(1.0)).flex_1().bg(theme.border_inactive))
        .child(
            div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .bg(theme.focus_ring),
        )
        .child(div().w(px(1.0)).flex_1().bg(theme.border_inactive));

    // Subject flex-grows but truncates. `w_full` on the outer row gives the
    // `flex_1` child a definite width to shrink against; without it taffy
    // hands the row its intrinsic (content) width and truncation never
    // engages — the long subject paints past the panel's right edge.
    //
    // `min_w(px(80))` is a hard floor so the subject can never collapse to
    // nothing under narrow-panel pressure. At 12px sans-serif that's ~11
    // characters before ellipsis — enough to keep the row meaningful at
    // any width the cockpit shell allows. Combined with `flex_shrink_0`
    // on the date/sha trailing columns and `overflow_hidden` on the row,
    // any remaining overflow clips from the right (sha first, then date),
    // giving the desired collapse priority without per-column shrink
    // factors that GPUI's Styled trait doesn't expose numerically.
    let subject = div()
        .flex_1()
        .min_w(px(80.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(sc_style::BODY_TEXT))
        .text_color(theme.fg_base)
        .child(c.subject.clone());

    // Author capped to ~88px so a long display name can't crowd out the
    // subject column. Date and SHA stay shrink-0 because they're naturally
    // short and always need to be readable.
    let author = div()
        .flex_shrink()
        .min_w(px(0.0))
        .max_w(px(88.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(sc_style::GRAPH_META_TEXT))
        .text_color(theme.fg_subtle)
        .child(c.author.clone());

    let date = div()
        .flex_shrink_0()
        .text_size(px(sc_style::GRAPH_META_TEXT))
        .text_color(theme.fg_subtle)
        .child(c.short_date.clone());

    // Short OID rendered in the typography mono face at 10px to match the
    // reference — the smaller monospaced rendering keeps the hash readable
    // without competing visually with the subject/author meta.
    let sha = div()
        .flex_shrink_0()
        .text_size(px(10.0))
        .font_family(typography.family_mono.clone())
        .text_color(theme.fg_subtle)
        .child(c.short_oid.clone());

    // Row has an explicit height so the timeline's flex_1 connector lines
    // have a definite parent height to distribute into. Without `h(…)` the
    // row collapses to text line-height and the connector lines shrink to a
    // near-invisible 2-3 px each.
    //
    // The hover tooltip surfaces the full subject (no truncation), the
    // commit body, and (once the per-commit numstat fetch resolves) the
    // +N/−N stat line, so a contributor can read the whole message and
    // size up the change without checking out the commit. Captures are
    // cheap `SharedString` clones.
    let tip_subject: SharedString = c.subject.clone().into();
    let tip_body: SharedString = c.body.clone().into();
    let tip_theme = theme;
    let tip_typography = typography.clone();
    let tip_stats = stats;

    // Row carries an `.id(...)` so the tooltip can attach: `.tooltip(...)`
    // lives on `StatefulInteractiveElement`, which div only implements
    // after an id is set. The short OID is unique per commit, so a
    // per-row id is both stable across renders and collision-free.
    let row_id = ElementId::Name(format!("graph-commit-{}", c.short_oid).into());
    let chips = render_ref_chips(&c.refs, &c.short_oid, theme, density, typography);

    // Capture-by-clone for the click closure — emits `ShowCommitRequested`
    // so the host workspace can open a commit-detail tab. The closure
    // holds `String` clones rather than borrowing the row's `CommitInfo`
    // because GPUI listeners outlive the render call's borrow stack.
    let click_sha = c.oid.clone();
    let click_short = c.short_oid.clone();
    let click_subject = c.subject.clone();

    // Right-click opens the shared `CommitContextMenu` mounted on
    // WorkspaceRoot (same pattern as the file-tree + git-row menus).
    // The full menu offers Cherry-pick / Revert / Copy SHA / Copy
    // short SHA; closure ships both forms in the action payload so
    // the menu doesn't have to re-derive the short prefix.
    let ctx_full = c.oid.clone();
    let ctx_short = c.short_oid.clone();

    div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .px(px(sc_style::PAD_H))
        .h(px(sc_style::COMMIT_ROW_H))
        .w_full()
        .overflow_hidden()
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .child(timeline.self_stretch())
        .child(subject)
        .when_some(chips, |row, chips| row.child(chips))
        .when(show_author, |row| row.child(author))
        .child(date)
        .child(sha)
        .tooltip(move |window, cx| {
            let subject = tip_subject.clone();
            let body = tip_body.clone();
            let theme = tip_theme;
            let typography = tip_typography.clone();
            let stats = tip_stats;
            Tooltip::element(move |_window, _cx| {
                render_commit_tooltip(
                    subject.clone(),
                    body.clone(),
                    stats,
                    theme,
                    typography.clone(),
                )
            })
            .build(window, cx)
        })
        .on_mouse_down(
            MouseButton::Right,
            move |ev: &MouseDownEvent, window, cx| {
                window.dispatch_action(
                    Box::new(crate::actions::OpenCommitContextMenuAt {
                        x: ev.position.x.into(),
                        y: ev.position.y.into(),
                        sha: ctx_full.clone(),
                        short_sha: ctx_short.clone(),
                    }),
                    cx,
                );
            },
        )
        .on_click(move |_: &ClickEvent, _window, cx| {
            let _ = weak.update(cx, |_graph, cx| {
                cx.emit(ShowCommitRequested {
                    sha: click_sha.clone(),
                    short_oid: click_short.clone(),
                    subject: click_subject.clone(),
                });
            });
        })
        .into_any_element()
}

/// Render the `RefLabel` chip cluster shown between subject and author
/// columns. `None` when the commit has no decorations — caller skips
/// the slot entirely so the row layout stays tight.
///
/// Cap at `REF_CHIPS_VISIBLE` visible chips; overflow collapses into a
/// `+N` chip whose tooltip lists the hidden refs. HEAD chip is
/// foregrounded with `status_info` so the current commit reads at a
/// glance.
fn render_ref_chips(
    refs: &[RefLabel],
    row_id: &str,
    theme: Theme,
    density: oximux_settings::Density,
    typography: &Typography,
) -> Option<gpui::Div> {
    if refs.is_empty() {
        return None;
    }
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .flex_shrink_0();
    let mut shown = 0usize;
    for r in refs.iter().take(REF_CHIPS_VISIBLE) {
        row = row.child(ref_chip_for(r, theme, density, typography));
        shown += 1;
    }
    let hidden = refs.len().saturating_sub(shown);
    if hidden > 0 {
        let overflow_text: SharedString = format!("+{hidden}").into();
        // Tooltip text lists every hidden ref's literal label; built
        // once at render so the closure doesn't have to re-walk.
        let hidden_labels: Vec<String> = refs
            .iter()
            .skip(shown)
            .map(ref_label_text)
            .collect();
        let tip_text: SharedString = hidden_labels.join(", ").into();
        // Per-commit unique id — keying on count alone collides when
        // two visible commits both have the same total ref count
        // (overflow chip tooltips would cross-pollute via shared
        // interactive state).
        let chip_id = ElementId::Name(format!("ref-chip-overflow-{row_id}").into());
        row = row.child(
            div()
                .id(chip_id)
                .px(px(6.0))
                .py(px(1.0))
                .rounded(px(density.r_chip))
                .bg(theme.bg_panel_alt)
                .text_size(px(sc_style::SUB_LABEL_TEXT))
                .text_color(theme.fg_muted)
                .child(overflow_text)
                .tooltip(move |window, cx| Tooltip::new(tip_text.clone()).build(window, cx)),
        );
    }
    Some(row)
}

/// Single chip for one `RefLabel`. HEAD pops in `status_info` blue;
/// branch tips, remote-tracking branches, and tags each pick their
/// own neutral tint so the cluster scans like a legend without
/// shouting.
fn ref_chip_for(
    r: &RefLabel,
    theme: Theme,
    density: oximux_settings::Density,
    typography: &Typography,
) -> gpui::Div {
    let (label, fg, bg) = match r {
        RefLabel::Head => (
            "HEAD".to_string(),
            theme.status_info,
            gpui::Hsla {
                a: 0.20,
                ..theme.status_info
            },
        ),
        RefLabel::BranchTip { name } => (name.clone(), theme.fg_base, theme.bg_panel_alt),
        RefLabel::RemoteBranch { name } => (name.clone(), theme.fg_muted, theme.bg_panel_alt),
        RefLabel::Tag { name } => (
            format!("⚑ {name}"),
            theme.status_warning,
            gpui::Hsla {
                a: 0.20,
                ..theme.status_warning
            },
        ),
        RefLabel::Other { raw } => (raw.clone(), theme.fg_subtle, theme.bg_panel_alt),
    };
    let _ = typography; // kept for future font-tweak hooks (mono vs sans).
    div()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(density.r_chip))
        .bg(bg)
        .text_size(px(sc_style::SUB_LABEL_TEXT))
        .text_color(fg)
        .child(label)
}

/// Render text for a ref's tooltip representation. Mirrors what
/// `git log --decorate` shows so the tooltip reads naturally to
/// anyone who's used the CLI.
fn ref_label_text(r: &RefLabel) -> String {
    match r {
        RefLabel::Head => "HEAD".to_string(),
        RefLabel::BranchTip { name } => name.clone(),
        RefLabel::RemoteBranch { name } => name.clone(),
        RefLabel::Tag { name } => format!("tag: {name}"),
        RefLabel::Other { raw } => raw.clone(),
    }
}

/// Build the multi-line tooltip body shown on commit-row hover. Subject sits
/// on its own as the title; the commit body — when present — renders below
/// it with the original line breaks preserved. Width is capped so long
/// subjects wrap instead of stretching the popover across the workspace.
///
/// `stats` is `Some((added, removed))` once the per-commit numstat
/// fetch has populated the cache. The stats render as a third
/// section under the body, `+N · −N`, using the repo's
/// `status_added` / `status_removed` theme tokens so the row reads
/// at a glance. Missing stats (first hover, root commit, or a
/// transient git failure) silently drop the line.
pub(super) fn render_commit_tooltip(
    subject: SharedString,
    body: SharedString,
    stats: Option<(u32, u32)>,
    theme: Theme,
    typography: Typography,
) -> impl IntoElement {
    // Width cap chosen so a typical 60-column body wraps once or twice
    // rather than producing a single very wide line.
    let max_width = px(440.0);

    let mut col = div()
        .flex()
        .flex_col()
        .max_w(max_width)
        .text_size(px(sc_style::BODY_TEXT))
        .text_color(theme.fg_base)
        .child(div().font_weight(typography.w_semibold).child(subject));

    if !body.is_empty() {
        // GPUI's text element doesn't split on `\n` itself, so we emit one
        // child per body line and substitute a small spacer for blank lines.
        // This preserves the original paragraph structure of the commit
        // message (bullet lists, code-block-style indents, footer trailers).
        let mut body_col = div()
            .flex()
            .flex_col()
            .pt(px(sc_style::PAD_V_TIGHT))
            .text_color(theme.fg_muted);
        for line in body.lines() {
            if line.is_empty() {
                body_col = body_col.child(div().h(px(6.0)));
            } else {
                body_col = body_col.child(div().child(line.to_string()));
            }
        }
        col = col.child(body_col);
    }

    if let Some((added, removed)) = stats {
        col = col.child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .pt(px(sc_style::PAD_V_TIGHT))
                .text_size(px(sc_style::GRAPH_META_TEXT))
                .child(
                    div()
                        .text_color(theme.status_added)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .text_color(theme.status_removed)
                        .child(format!("−{removed}")),
                ),
        );
    }

    col
}
