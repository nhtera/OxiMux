//! One stash row: message, then `branch · relative`, then the file count,
//! then the action cluster.
//!
//! # Two truncating siblings, one growing
//!
//! The message and the metadata both shrink, so a long message cannot push the
//! metadata — or the actions — off the panel. They do not shrink equally: the
//! message yields first (shrink [`MESSAGE_SHRINK`] against the metadata's
//! [`META_SHRINK`]) because a branch name and a date are short, fixed-length
//! and scannable, while a message is long and already summarised by its first
//! few words. The metadata carries `min_w(0)`, without which a flex child's
//! automatic min-content floor refuses to shrink below the text's natural
//! width and the ellipsis never appears (the trap `mod.rs` hit first at its
//! old single-label row). The message carries [`MESSAGE_MIN_EMS`] instead,
//! because shrink weighting alone does not keep a SHORT message whole — see
//! that constant.
//!
//! The metadata slot is also the only child that GROWS, so it absorbs the
//! slack and everything after it sits against the right edge. That job is
//! deliberately not done with `ml_auto` on the count and on the action
//! cluster: flexbox splits free space EQUALLY between auto margins, so two of
//! them would park the count halfway across the row.
//!
//! # The chevron and the count
//!
//! The chevron toggles the file list; `file_row.rs` paints what it reveals.
//! It is the ONLY thing in the row that expands — the row body does not,
//! because the trailing cluster is three live verbs and a mis-aimed click
//! that silently changes the layout under the cursor is worse than a click
//! that does nothing.
//!
//! The count is `Option`, rendered only when `Some`. The file list is fetched
//! lazily on expand, so a row nobody has expanded knows nothing about its
//! files, and a confident `0` would be a claim we have not earned.
//!
//! # Hidden until hover, because the menu now exists
//!
//! The cluster used to rest at 45% opacity rather than hidden, for one
//! documented reason: the panel had no context menu, so hiding it would have
//! left Drop with no other way in. [`super::context_menu`] removes that
//! premise, so the cluster takes the repo's ordinary hidden-until-hover row
//! treatment and the row at rest is finally just a stash.
//!
//! The icons are NOT gpui-component `Button`s. An icon-only `.xsmall()`
//! Button is `size_5` — a flat 20px that ignores both the density preset and
//! the zoom — which is exactly why this row was stuck at `h_action_row`
//! through Phase 4. [`icon_action`] sizes itself from `ScmStyle::icon`, so it
//! fits inside `density.h_row` at every zoom and the row can finally take the
//! tighter height.
//!
//! The cluster is positioned absolutely, not laid out as the row's last
//! child — at 220px and 150% the row cannot afford it either way, so it is
//! taken out of the width arithmetic entirely. See the cluster itself.
//!
//! Only the verbs that exist are painted: Apply, Pop, Drop. Branch-from-stash
//! and Rename belong to later phases and are absent rather than present and
//! dead — a button that does nothing is the defect Drop already shipped once.

use crate::shell::stash_panel::list_render::{row_message, row_meta, row_tooltip};
use crate::shell::stash_panel::{DropStashRequested, StashPanel};
use crate::shell::source_control::style::ScmStyle;
use gpui::{
    App, ClickEvent, Context, ElementId, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{Icon, tooltip::Tooltip};
use oximux_core::StashEntry;
use oximux_settings::Theme;

/// Hit area of one row-action icon button, as a multiple of the glyph inside
/// it. A ratio rather than a size: `ScmStyle::icon` already carries the zoom,
/// so the button grows with the row instead of staying a 20px literal the way
/// an `.xsmall()` Button does. 1.6 leaves ~4px of padding per side at the
/// default and still clears `density.h_row` at every preset.
const ICON_BUTTON_RATIO: f32 = 1.6;

/// Shrink factors, not sizes: the message gives up width twice as fast as the
/// metadata, down to [`MESSAGE_MIN_EMS`], because a branch name and a date are
/// short and fixed-length while a subject is already summarised by its first
/// few words.
const MESSAGE_SHRINK: f32 = 2.0;
const META_SHRINK: f32 = 1.0;

/// Floor under the message column, in multiples of the body type size —
/// roughly fourteen characters.
///
/// Flex shrink alone does not protect it: the deficit is split in proportion
/// to each sibling's natural width, so a four-character message gives up the
/// same FRACTION as a long one and `temp` renders as `te…` while the branch
/// beside it sits there in full. Verified live at the shipped panel width.
/// A floor also lines the metadata up into a column across rows, which is how
/// the approved design reads.
const MESSAGE_MIN_EMS: f32 = 7.0;

impl StashPanel {
    pub(super) fn render_row(&self, entry: StashEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let style = ScmStyle::new(density, typography);
        let index = entry.stash_ref.index;
        let expanded = self.is_expanded(&entry.sha);
        let file_count = self.file_count(&entry.sha);

        let message = row_message(&entry);
        let meta = row_meta(&entry);
        let tooltip = row_tooltip(&entry, file_count);

        // Ops are keyed by sha, not by the index this row is painted with:
        // the stack is shared with every worktree and with the user's
        // terminal, so `index` can address a different stash by the time a
        // click lands. See `ops.rs`.
        let apply_sha = entry.sha.clone();
        let pop_sha = entry.sha.clone();
        let toggle_sha = entry.sha.clone();
        let drop_entry = entry.clone();
        let menu_entry = entry.clone();
        // Hover scope for the progressive-disclosure cluster below, and the
        // row's own stateful id (which `.tooltip` needs).
        //
        // Sha FIRST so neither is reattached to a different stash when the
        // stack shifts; index appended because a sha is not unique on the
        // stack — `git stash store` can park one commit at two addresses, and
        // two rows sharing an id share element state and hover as one. See
        // `resolve_stash_index`.
        let key = format!("{}-{index}", entry.sha);
        let group_name = format!("stash-row-{key}");
        let row_id = ElementId::Name(format!("stash-row-{key}").into());

        // Apply ↧ / Pop ↥ / Drop 🗑. The two arrows are mirror images on
        // purpose: apply and pop differ only in whether the entry survives,
        // and a user who learns one glyph has learned the other.
        //
        // # Out of the flex flow, on purpose
        //
        // The cluster is absolutely positioned against the row's right edge
        // rather than laid out as its last child. A flex child has to be
        // PAID FOR out of the row's width, and at the panel's 220px minimum
        // with the UI at 150% the row cannot afford it: chrome, chevron, gaps
        // and the message's own [`MESSAGE_MIN_EMS`] floor already come to
        // more than 220px, so the overflow leaves the row — and the window —
        // taking Pop and Drop with it. Measured live at exactly that
        // combination, first with three text buttons (Phase 4) and again with
        // these three icons, which are a third of the width and still did not
        // fit. Shrinking the cluster was never going to be the fix.
        //
        // Absolute makes reachability a floor instead of an arithmetic race:
        // whatever the width, the cluster sits inside the row's right edge,
        // and what it costs is covering the tail of the metadata *while the
        // pointer is on the row*. Hence the opaque `bg_panel` backing — text
        // showing through the glyphs reads as a rendering fault — and the
        // left padding, so the covered text ends in air rather than against
        // the first icon.
        //
        // Hidden at rest, revealed on row-hover. `.invisible()` rather than
        // dropping the children, so hover reveals rather than inserts.
        let actions = div()
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(density.pad_panel))
            .flex()
            .flex_row()
            .items_center()
            .pl(px(density.gap_inline))
            .bg(theme.bg_panel)
            .gap(px(style.icon_cluster_gap))
            .invisible()
            .group_hover(group_name.clone(), |s| s.visible())
            .child(icon_action(
                ElementId::Name(format!("stash-apply-{key}").into()),
                "icons/arrow-down.svg",
                "Apply stash (keep it in the list)",
                theme.fg_muted,
                theme,
                style,
                cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                    // The row applies plainly. `--index` is a menu item, not
                    // the default — see `StashPanel::apply`.
                    panel.apply(apply_sha.clone(), index, false, cx);
                    cx.notify();
                }),
            ))
            .child(icon_action(
                ElementId::Name(format!("stash-pop-{key}").into()),
                "icons/arrow-up.svg",
                // NOT "reversible via reflog" — a pop deletes the stash's
                // reflog entry, leaving the commit sha as the only way back.
                "Apply stash and remove it from the list",
                theme.fg_muted,
                theme,
                style,
                cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                    panel.pop(pop_sha.clone(), index, cx);
                    cx.notify();
                }),
            ))
            .child(icon_action(
                ElementId::Name(format!("stash-drop-{key}").into()),
                "icons/trash.svg",
                "Drop this stash",
                // The one tinted glyph in the row. `danger_ghost` exists
                // because a gpui-component Button overwrites `text_color`
                // from its variant table; a plain svg has no such table, so
                // the tint is just the colour we pass.
                theme.status_error,
                theme,
                style,
                cx.listener(move |_panel, _: &ClickEvent, _window, cx| {
                    // The panel never drops on its own — the host owns the
                    // confirm step and calls back into `drop_confirmed`.
                    cx.emit(DropStashRequested {
                        stash_ref: drop_entry.stash_ref.clone(),
                        sha: drop_entry.sha.clone(),
                        message: drop_entry.message.clone(),
                        relative: drop_entry.relative.clone(),
                        branch: drop_entry.branch.clone(),
                    });
                }),
            ));

        // Points right when closed, down when open — the same convention the
        // section header above and every other collapsible in the panel use.
        let chevron = div()
            .id(ElementId::Name(format!("stash-expand-{key}").into()))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .size(px(style.icon))
            .cursor_pointer()
            .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                panel.toggle_expanded(toggle_sha.clone(), cx);
                cx.stop_propagation();
            }))
            .child(
                Icon::default()
                    .path(if expanded {
                        "icons/chevron-down.svg"
                    } else {
                        "icons/chevron-right.svg"
                    })
                    .size(px(style.icon))
                    .text_color(theme.fg_muted),
            );

        let row = div()
            .id(row_id)
            .group(group_name)
            // Anchors the absolutely-positioned action cluster above. Safe to
            // add here — this element never positions itself, so there is no
            // `.absolute()` for `.relative()` to clobber.
            .relative()
            .flex()
            .flex_row()
            .items_center()
            // `h_row`, not `h_action_row`. The taller height was never a
            // design choice — it was the smallest thing that would hold a
            // 22px `danger_ghost` and two 20px Buttons, none of which scale.
            // With `icon_action` sized from the glyph, the row is a list row
            // again and reads level with the file rows under it.
            .h(px(density.h_row))
            .px(px(density.pad_panel))
            .gap(px(density.gap_inline))
            // The message floor makes this row un-compressible past a point,
            // and the SCM panel drags down to 220px
            // (`scm_layout_settings::MIN_PANEL_WIDTH`) — a constant that does
            // NOT scale with zoom, while the type and padding around it do. At
            // the minimum width and 150% the action cluster no longer fits, and
            // the scroll parent masks the Y axis only, so nothing else here
            // clips the X overflow. This row owns that, like `graph_row.rs:165`.
            //
            // Honest scope: with the panel docked flush to the window's right
            // edge, the overflow leaves the WINDOW and is clipped there anyway
            // — verified by negative control at 220px/150%, where removing this
            // line changed no pixel by more than 2/255. It is a guard against
            // the row's own geometry, not a fix for a defect visible today, and
            // it holds if anything is ever placed to the section's right. The
            // reachability half of that report — Pop and Drop falling off the
            // edge at 220px/150% — is fixed by taking the cluster OUT of this
            // row's flex flow, not by making it narrower; three glyphs are a
            // third of the width three labels were and still did not fit.
            .overflow_hidden()
            .border_b_1()
            .border_color(theme.border_inactive)
            .child(chevron)
            .child(
                div()
                    .flex_shrink(MESSAGE_SHRINK)
                    .min_w(px(style.body_text * MESSAGE_MIN_EMS))
                    .truncate()
                    .text_size(px(style.body_text))
                    .text_color(theme.fg_base)
                    .child(message),
            )
            .child(
                // Rendered even when empty, and the only thing in the row that
                // GROWS: it takes the slack, which is what pins the count and
                // the action cluster to the right edge. An `ml_auto` on each of
                // those would not — flexbox splits free space EQUALLY between
                // auto margins, so the count would drift to the middle as soon
                // as Phase 5 gives it a value.
                div()
                    .flex_grow(1.0)
                    .flex_shrink(META_SHRINK)
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(style.graph_meta_text))
                    .text_color(theme.fg_muted)
                    .child(meta),
            )
            .when_some(file_count, |row, count| {
                // Plain dimmed number, no chip — the convention the branch
                // section's header count already uses (`branch_commits.rs:167`).
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(style.graph_meta_text))
                        .text_color(theme.fg_subtle)
                        .child(format!("{count}")),
                )
            })
            .child(actions)
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            // Right-click opens the full verb list. This is the guarantee
            // that lets the cluster above hide: every action the row offers,
            // plus the ones no row has space for, are reachable without
            // hovering — which is what a touch or keyboard user has.
            .on_mouse_down(
                MouseButton::Right,
                move |ev: &MouseDownEvent, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::actions::OpenStashContextMenuAt {
                            x: ev.position.x.into(),
                            y: ev.position.y.into(),
                            sha: menu_entry.sha.clone(),
                            index,
                            message: menu_entry.message.clone(),
                            relative: menu_entry.relative.clone(),
                            branch: menu_entry.branch.clone(),
                            file_path: None,
                        }),
                        cx,
                    );
                },
            );

        // The row and its expansion are one column so the caller's list loop
        // stays a loop over stashes rather than a loop that has to know
        // whether each one is open.
        div()
            .flex()
            .flex_col()
            .w_full()
            .child(row)
            .when(expanded, |col| col.child(self.render_files(&entry, cx)))
    }
}

/// One icon-only row action: a square hit area, a tinted glyph, a tooltip.
///
/// Deliberately not a gpui-component `Button`. Two reasons, both of which
/// this row hit before:
///
/// * An icon-only `.xsmall()` Button is `size_5` — 20 flat pixels that follow
///   neither the density preset nor the zoom. Sizing from `ScmStyle::icon`
///   keeps the control inside `density.h_row` at 80% and at 200%.
/// * A Button resolves `text_color` from its variant's style table at render
///   time, so `Styled::text_color` cannot paint one in `status_error`. That
///   is the whole reason `oximux_ui::danger_ghost` exists; a plain `svg` has
///   no table to lose to, so Drop's red is just the colour passed in.
///
/// The tooltip is not decoration — it is the only label an icon-only control
/// has, which is why every caller passes one.
fn icon_action<H>(
    id: ElementId,
    icon: &'static str,
    tooltip: &'static str,
    color: Hsla,
    theme: Theme,
    style: ScmStyle,
    on_click: H,
) -> impl IntoElement
where
    H: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(px(style.icon * ICON_BUTTON_RATIO))
        .rounded(px(style.corner))
        .cursor_pointer()
        .hover(move |s| s.bg(theme.hover_overlay))
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .on_click(on_click)
        .child(
            // `text_color` explicitly: an `Icon` with none inherits nothing
            // useful and paints invisible.
            Icon::default().path(icon).size(px(style.icon)).text_color(color),
        )
}
