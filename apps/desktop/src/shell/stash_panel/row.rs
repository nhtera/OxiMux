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
//! # What this row deliberately does not paint yet
//!
//! * **The tighter `h_row` height.** The row still carries three text buttons
//!   — 20px for the two xsmall `Button`s, 22px for `danger_ghost`, whose
//!   height is a literal that does not scale. `h_row` is 24px at cockpit
//!   density and 19.2px at the 80% minimum zoom, so the tighter row would
//!   clip a control that cannot shrink. The height moves when Phase 6
//!   replaces the cluster with icons — and only if the icon buttons scale.

use crate::shell::stash_panel::list_render::{row_message, row_meta, row_tooltip};
use crate::shell::stash_panel::{DropStashRequested, StashPanel};
use crate::shell::source_control::style::ScmStyle;
use crate::ui::danger_ghost;
use gpui::{
    ClickEvent, Context, ElementId, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants},
    tooltip::Tooltip,
};
use oximux_core::StashEntry;

/// Resting opacity of a stash row's Apply/Pop/Drop cluster — ghosted enough to
/// calm the row, present enough that the actions are always discoverable and
/// clickable (the panel has no context-menu fallback). Lifts to full on
/// row-hover.
const STASH_ACTION_REST_OPACITY: f32 = 0.45;

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

        // Apply / Pop / Drop all sit at xsmall height (20px for the two
        // Buttons, 22px for `danger_ghost`) so the row reads as one action
        // cluster — the destructive verb doesn't dominate by being larger
        // than its siblings.
        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            // Pin the action cluster: it must never shrink or clip — a narrow
            // panel truncates the text instead (the canonical SCM-row collapse
            // priority). Without this, a long stash message pushed the cluster
            // off the right edge and clipped "Drop".
            .flex_shrink_0()
            .gap(px(density.gap_inline))
            // Progressive disclosure: the cluster rests ghosted and lifts to
            // full on row-hover, so a calm row at rest but every verb is one
            // hover away. NOT fully hidden on purpose — the stash panel has no
            // context menu, so a hidden cluster would leave Drop (destructive)
            // with no alternative invocation path. Ghost-at-rest keeps every
            // action reachable at all times (documented exception to the
            // fully-hidden row-action convention used where a context-menu
            // backup exists).
            .opacity(STASH_ACTION_REST_OPACITY)
            .group_hover(group_name.clone(), |s| s.opacity(1.0))
            .child(
                Button::new(("stash-apply", index))
                    .ghost()
                    .xsmall()
                    .label("Apply")
                    .tooltip("Apply stash (keep it in the list)")
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                        panel.apply(apply_sha.clone(), index, cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new(("stash-pop", index))
                    .ghost()
                    .xsmall()
                    .label("Pop")
                    // NOT "reversible via reflog" — a pop deletes the stash's
                    // reflog entry, leaving the commit sha as the only way back.
                    .tooltip("Apply stash and remove it from the list")
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                        panel.pop(pop_sha.clone(), index, cx);
                        cx.notify();
                    })),
            )
            .child(danger_ghost(
                ("stash-drop", index),
                "Drop",
                &theme,
                &density,
                typography,
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
            .flex()
            .flex_row()
            .items_center()
            .h(px(density.h_action_row))
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
            // it holds if anything is ever placed to the section's right. What
            // IS wrong at that size is that Pop and Drop fall off the edge
            // entirely; only Phase 6's icon cluster fixes reachability.
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
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx));

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
