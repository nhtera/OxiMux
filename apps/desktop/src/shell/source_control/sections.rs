//! The one place that can see both resizable SCM sections at once.
//!
//! The stash list and the commit graph are separate entities with separate
//! heights, separate drag handles and separate settings keys — but they are
//! both `flex_shrink_0` children of a column whose only flexible child is the
//! changed-files block. Every pixel either takes comes out of that list, which
//! stops giving at `files_floor`; past that the sections themselves overflow
//! the column's `overflow_hidden` and are clipped, taking the lower section's
//! drag handle — the escape valve a cramped panel depends on — off-screen.
//!
//! Neither section can prevent that alone. `CommitGraph::apply_graph_drag`
//! runs on data `CommitGraph` owns and has no `stash_h` in scope; the stash
//! panel's rail has no `graph_h`. An implementer told to "clamp against the
//! other section" from inside either one finds nothing to clamp against,
//! passes zero, and the clamp silently does nothing.
//!
//! So the budget is arbitrated here, by the parent that holds both entities,
//! and pushed down to each section as a plain ceiling.
//!
//! # Pushed every render, not only on drag
//!
//! The drag is not the only way the budget changes. So do: the keyboard rails
//! (which read `window.bounds()` and know nothing of a sibling), a window
//! resize, collapsing either section, a scope switch that hides the graph
//! entirely, and a pair of heights restored from settings that were chosen on
//! a taller monitor. A ceiling refreshed only on drag would miss every one of
//! those. [`SourceControlPanel::sync_section_budget`] runs on the render path,
//! which is the one thing all of them have in common.
//!
//! The push sets a ceiling; it never writes a trimmed height back into either
//! section's state. What the user dragged to is what they asked for, and a
//! temporarily short window must not quietly forget it — each section paints
//! `min(chosen, ceiling)` and springs back when the room returns.

use crate::scm_layout_settings;
use crate::shell::source_control::SourceControlPanel;
use gpui::{AnyElement, Context, IntoElement, ParentElement, Pixels, Styled, div, px};
use oximux_settings::Theme;
use oximux_storage::SettingsRepo;

/// Both sections' persisted heights, as `(stash, graph)`.
///
/// Read before either section mounts, so each appears at the size the user
/// left it rather than snapping after the first paint. Each is clamped
/// against the live window on the way out, so a height chosen on a taller
/// monitor cannot overflow a shorter one; the pair is then reconciled against
/// their shared budget on every render by [`SourceControlPanel::sync_section_budget`].
///
/// `None` is the test wiring — defaults, and nothing persisted.
pub(crate) fn initial_heights(
    settings_repo: Option<&SettingsRepo>,
    window_height: f32,
) -> (Pixels, Pixels) {
    match settings_repo {
        Some(repo) => (
            px(scm_layout_settings::load_stash_height(repo, window_height)),
            px(scm_layout_settings::load_graph_height(repo, window_height)),
        ),
        None => (
            px(scm_layout_settings::DEFAULT_STASH_HEIGHT),
            px(scm_layout_settings::DEFAULT_GRAPH_HEIGHT),
        ),
    }
}

/// Which resizable section a drag tick belongs to. The workspace root selects
/// drags by payload type and translates the payload into one of these, so the
/// routing below is the only code that has to know there are two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmSection {
    Stash,
    Graph,
}

impl SourceControlPanel {
    /// Recompute each section's ceiling from its sibling and hand it down.
    ///
    /// Called from `render`. Deliberately does not notify: the ceiling only
    /// constrains future mutations and the height painted in this same pass,
    /// so notifying would be a render-triggered re-render with nothing new to
    /// say (and `notify()` during a render pass is dropped anyway).
    pub(crate) fn sync_section_budget(&mut self, window_height: f32, cx: &mut Context<Self>) {
        let graph = self.commit_graph.clone();
        let stash = self.stash_panel.clone();
        // A section hidden by the current scope, or collapsed to its header,
        // costs its sibling nothing — `None` rather than `0.0` so the
        // distinction is in the type instead of in a sentinel.
        //
        // These are the heights the user CHOSE, not the ones being painted.
        // Feeding painted heights back in closes a loop through the render
        // pass and the two sections flicker — see `fit_sections`.
        let graph_chosen = self
            .scope
            .shows_graph()
            .then(|| graph.read(cx).chosen_height())
            .flatten();
        let stash_chosen = stash.read(cx).chosen_height();

        let (stash_ceiling, graph_ceiling) =
            scm_layout_settings::fit_sections(stash_chosen, graph_chosen, window_height);
        stash.update(cx, |p, _| p.set_section_ceiling(stash_ceiling));
        graph.update(cx, |g, _| g.set_section_ceiling(graph_ceiling));
    }

    /// Route one drag tick to the section it belongs to.
    ///
    /// Both sections' handles come through here rather than each reaching its
    /// own entity from the workspace root, so the pair has one owner and a
    /// third section is a match arm rather than a fourth listener chain.
    pub fn apply_section_drag(
        &mut self,
        section: ScmSection,
        cursor_y: f32,
        window_height: f32,
        cx: &mut Context<Self>,
    ) {
        // The ceilings were set on the last render; a drag tick cannot change
        // the sibling, so they are still current.
        match section {
            ScmSection::Stash => {
                let stash = self.stash_panel.clone();
                stash.update(cx, |p, cx| p.apply_stash_drag(cursor_y, window_height, cx));
            }
            ScmSection::Graph => {
                let graph = self.commit_graph.clone();
                graph.update(cx, |g, cx| g.apply_graph_drag(cursor_y, window_height, cx));
            }
        }
    }
}

/// Frame one resizable section in the SCM column.
///
/// The top hairline is not decoration: without a rule between them the file
/// list and the section below read as one colliding surface even when they no
/// longer collide, which is how the original clip got reported as an overlap.
///
/// # Why this shrinks, when the sections themselves do not
///
/// The changed-files block absorbs the squeeze first — that is the column's
/// collapse priority and it is why the sections are sized, not flexed. But it
/// stops absorbing at `files_floor`, and with nothing else able to give, the
/// remaining deficit used to leave the column entirely: dragging the stash
/// section to its ceiling pushed the GRAPH past `overflow_hidden` and off the
/// panel, drag handle and all. Verified live, at the default window and panel
/// width.
///
/// A ratio-of-the-window budget cannot prevent that, because the space these
/// two sections are actually competing for is the column minus its chrome —
/// tabs, toolbar, filter row, composer, conflict and checks banners — and how
/// tall that chrome is depends on the scope and on the repo's state, neither
/// of which a settings constant can see. So the budget stays as the thing
/// that shapes the *gesture*, and this is the floor under it: `flex_shrink`
/// with a `min_h`, which makes "one section is pushed out of the column" a
/// shape flexbox cannot produce, whatever the chrome above happens to cost.
///
/// `min_h` is the section's own minimum, so the squeeze stops while the
/// section is still a usable strip with its handle attached; `overflow_hidden`
/// keeps the compressed section inside its frame rather than over its
/// neighbour.
pub(crate) fn frame(section: impl IntoElement, theme: Theme, min_h: f32) -> AnyElement {
    div()
        .flex_shrink(1.0)
        .min_h(px(min_h))
        .overflow_hidden()
        .border_t_1()
        .border_color(theme.border_inactive)
        .child(section)
        .into_any_element()
}
