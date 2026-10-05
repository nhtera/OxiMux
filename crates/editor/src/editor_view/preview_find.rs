//! The markdown preview's search engine, behind the shared find widget
//! ([`super::find_bar`]).
//!
//! Matching runs over the text the reader *sees*, not the markdown source:
//! `**bold**` renders as `bold`, and list markers, fences and link
//! destinations render as nothing, so a source search would both miss hits
//! that cross formatting and find text that is not on screen. The renderer
//! hands that text out as [`TextViewState::rendered_text`] and takes the hits
//! back as [`RangeHighlight`]s, which paint behind the glyphs without touching
//! layout, selection or copy; `reveal_range` scrolls the current one into
//! view. All three arrived in gpui-kit 0.7 (`longbridge/gpui-kit#3215`,
//! `#3216`). Before that the renderer kept its text and its state to itself,
//! which is why the preview had no find at all.

use std::ops::Range;

use gpui::{AppContext as _, Context, Entity, Hsla, Subscription};
use gpui_component::{
    ActiveTheme as _,
    text::{RangeHighlight, TextViewState},
};

use super::{EditorView, MarkdownViewMode, find_bar::FindTarget};

/// The preview renderer's state.
///
/// Owned by the view rather than keyed inside the `TextView` element (what
/// `TextView::markdown(id, text)` does) because find has to read the rendered
/// text and set highlights on it, and an element-keyed state is unreachable
/// from outside the element.
pub(super) struct PreviewText {
    pub(super) state: Entity<TextViewState>,
    /// Re-runs an open search when a new parse lands (an edit in Split, a
    /// mermaid diagram finishing): the old match ranges index text that is
    /// gone, and the renderer would reject them.
    _observe: Subscription,
}

/// Background tones for the current match and every other one.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct MatchTones {
    current: Hsla,
    other: Hsla,
}

/// The preview's match tones, both from the theme's `match_bg_current` amber
/// (the terminal search's "you are here" hue), as washes rather than solids.
///
/// A [`RangeHighlight`] paints only a background and the text keeps its own
/// colour, so the terminal's pair — a solid amber meant for dark text, and
/// `match_bg_other`, a grey that nearly vanishes on the dark preview and on
/// its code blocks — does not carry over: light text on solid amber is hard
/// to read. On dark, a translucent amber keeps the light text legible and
/// the others a dimmer tint of the same hue; on light, the opaque amber
/// already sits behind dark text, and a pale wash marks the rest.
fn match_tones(amber: Hsla, is_dark: bool) -> MatchTones {
    let (current, other) = if is_dark { (0.55, 0.28) } else { (1.0, 0.4) };
    MatchTones {
        current: amber.opacity(current),
        other: amber.opacity(other),
    }
}

/// Byte ranges of every non-overlapping occurrence of `query` in `haystack`,
/// in order. An empty query matches nothing.
///
/// Case-insensitive unless `match_case`. Folds with `char::to_lowercase`
/// rather than lowercasing both strings and using `str::find`, because
/// lowercasing can change a string's byte length (`İ` is 2 bytes, its
/// lowercase 3) and the ranges must index `haystack` itself — they go straight
/// to the renderer, which rejects any range that is not on a character
/// boundary. A match must start and end on whole source characters, so the
/// tail of one expanded character never matches.
pub(crate) fn find_matches(haystack: &str, query: &str, match_case: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    if match_case {
        return haystack
            .match_indices(query)
            .map(|(start, hit)| start..start + hit.len())
            .collect();
    }
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    // Each folded char with the byte range of the source char it came from.
    let folded: Vec<(char, Range<usize>)> = haystack
        .char_indices()
        .flat_map(|(start, c)| {
            let range = start..start + c.len_utf8();
            c.to_lowercase().map(move |l| (l, range.clone()))
        })
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= folded.len() {
        let window = &folded[i..i + needle.len()];
        let starts_whole = i == 0 || folded[i - 1].1 != window[0].1;
        let ends_whole = folded
            .get(i + needle.len())
            .is_none_or(|next| next.1 != window[needle.len() - 1].1);
        if starts_whole && ends_whole && window.iter().map(|(c, _)| *c).eq(needle.iter().copied())
        {
            out.push(window[0].1.start..window[needle.len() - 1].1.end);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

impl EditorView {
    /// The preview renderer's state, created on first use, holding `source`
    /// (already image-path-absolutized) as its text. `set_text` is a no-op
    /// for unchanged text, so calling this every render costs one compare.
    pub(super) fn sync_preview_text(
        &mut self,
        source: &str,
        cx: &mut Context<Self>,
    ) -> Entity<TextViewState> {
        let state = match &self.md_preview {
            Some(preview) => preview.state.clone(),
            None => {
                let state = cx.new(|cx| TextViewState::markdown(source, cx));
                let observe = cx.observe(&state, |this, _, cx| this.refresh_preview_find(false, cx));
                self.md_preview = Some(PreviewText { state: state.clone(), _observe: observe });
                state
            }
        };
        state.update(cx, |state, cx| state.set_text(source, cx));
        // A small replacement parses synchronously inside `set_text`, and this
        // runs during render, where gpui drops the parse's notify (no redraw
        // is scheduled mid-draw, so no observer runs). Check here as well;
        // when nothing re-parsed this is one snapshot comparison.
        self.refresh_preview_find(false, cx);
        state
    }

    /// Whether the rendered preview is on screen (Preview or Split).
    pub(super) fn preview_visible(&self) -> bool {
        self.is_markdown && matches!(self.md_mode, MarkdownViewMode::Preview | MarkdownViewMode::Split)
    }

    /// Search the rendered text again, when the open find targets the preview.
    ///
    /// `query_changed`: the query (or match-case) was edited, so restart at
    /// the first match and scroll to it. Otherwise the preview notified, which
    /// it also does for hover and selection; only a new parse (a different
    /// rendered-text snapshot) is worth a search, and that one keeps the
    /// cursor where it was and leaves the scroll alone, so an edit in Split
    /// does not yank the preview around under the reader.
    pub(super) fn refresh_preview_find(&mut self, query_changed: bool, cx: &mut Context<Self>) {
        let (Some(find), Some(preview)) = (&mut self.find, &self.md_preview) else {
            return;
        };
        if find.target != FindTarget::Preview {
            return;
        }
        let text = preview.state.read(cx).rendered_text();
        if !query_changed && find.searched.as_ref() == Some(&text) {
            return;
        }
        let query = find.query.read(cx).value().to_string();
        find.matches = find_matches(text.as_str(), &query, find.match_case);
        find.current = if query_changed {
            0
        } else {
            find.current.min(find.matches.len().saturating_sub(1))
        };
        find.searched = Some(text);
        self.apply_preview_find(query_changed, cx);
    }

    /// Move to the next (`dir = 1`) or previous (`dir = -1`) preview match,
    /// wrapping, and scroll it into view.
    pub(super) fn step_preview_find(&mut self, dir: isize, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let total = find.matches.len() as isize;
        find.current = (find.current as isize + dir).rem_euclid(total) as usize;
        self.apply_preview_find(true, cx);
    }

    /// Hand the matches to the renderer as highlights, the current one in the
    /// stronger tone, and optionally scroll to it.
    pub(super) fn apply_preview_find(&mut self, reveal: bool, cx: &mut Context<Self>) {
        let tones = self.preview_match_tones(cx);
        let (Some(find), Some(preview)) = (&mut self.find, &self.md_preview) else {
            return;
        };
        find.tones = Some(tones);
        let highlights: Vec<RangeHighlight> = find
            .matches
            .iter()
            .enumerate()
            .map(|(ix, range)| {
                let tone = if ix == find.current { tones.current } else { tones.other };
                RangeHighlight::new(range.clone(), tone)
            })
            .collect();
        let current = find.matches.get(find.current).cloned();
        preview.state.update(cx, |state, cx| {
            // The ranges came from the snapshot just read, so a rejection here
            // is a renderer contract change, not a user-visible state: log it
            // and leave the document unmarked rather than panic.
            if let Err(err) = state.set_range_highlights(highlights, cx) {
                tracing::debug!(%err, "markdown preview find: highlights rejected");
            }
            if reveal
                && let Some(range) = current
                && let Err(err) = state.reveal_range(range, cx)
            {
                tracing::debug!(%err, "markdown preview find: reveal rejected");
            }
        });
        cx.notify();
    }

    /// Repaint the preview highlights when the theme changed under them —
    /// without it the old theme's tones stay on the page (the dark theme's
    /// washes read as murky boxes on white). Called every render; a no-op
    /// unless the tones moved.
    pub(super) fn repaint_stale_preview_tones(&mut self, cx: &mut Context<Self>) {
        let tones = self.preview_match_tones(cx);
        let stale = self.find.as_ref().is_some_and(|find| {
            find.target == FindTarget::Preview && find.tones.is_some_and(|t| t != tones)
        });
        if stale {
            self.apply_preview_find(false, cx);
        }
    }

    /// Remove the preview's match highlights.
    pub(super) fn clear_preview_highlights(&self, cx: &mut Context<Self>) {
        if let Some(preview) = &self.md_preview {
            preview.state.update(cx, |state, cx| state.clear_range_highlights(cx));
        }
    }

    /// The match tones for the theme in force.
    fn preview_match_tones(&self, cx: &gpui::App) -> MatchTones {
        let amber = oximux_settings::appearance::theme(cx).match_bg_current;
        match_tones(amber, cx.theme().is_dark())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Window;

    #[test]
    fn matches_case_insensitively_without_overlap() {
        assert_eq!(find_matches("Overview of the overview", "overview", false), vec![0..8, 16..24]);
        assert_eq!(find_matches("aaaa", "aa", false), vec![0..2, 2..4]);
        assert_eq!(find_matches("abc", "ABC", false), vec![0..3]);
    }

    #[test]
    fn empty_query_and_no_hit_match_nothing() {
        assert!(find_matches("anything", "", false).is_empty());
        assert!(find_matches("anything", "zzz", false).is_empty());
        assert!(find_matches("", "a", false).is_empty());
    }

    #[test]
    fn ranges_index_the_original_text_across_multibyte_chars() {
        let text = "日本語 Überblick über";
        let hits = find_matches(text, "über", false);
        assert_eq!(hits.len(), 2);
        for hit in &hits {
            assert_eq!(text[hit.clone()].to_lowercase(), "über");
        }
    }

    #[test]
    fn a_length_changing_fold_keeps_ranges_on_char_boundaries() {
        // `İ` (2 bytes) lowercases to `i̇` (3 bytes, two chars), so a search on
        // a lowercased copy would hand back ranges shifted off the original.
        let text = "İstanbul and istanbul";
        let hits = find_matches(text, "stanbul", false);
        assert_eq!(hits.len(), 2);
        for hit in &hits {
            assert!(text.is_char_boundary(hit.start) && text.is_char_boundary(hit.end));
            assert_eq!(&text[hit.clone()], "stanbul");
        }
        // A whole-character query still matches the expanded character.
        assert_eq!(find_matches(text, "i̇stanbul", false), vec![0.."İstanbul".len()]);
    }

    #[test]
    fn a_match_never_starts_inside_an_expanded_character() {
        // The combining dot is the second folded char of `İ`; matching it alone
        // would produce a range that is half of one source character.
        assert!(find_matches("İ", "\u{307}", false).is_empty());
    }

    /// The matches index the text the renderer shows, and a type into the bar.
    fn search(view: &mut EditorView, query: &str, window: &mut Window, cx: &mut Context<EditorView>) {
        let input = view.find.as_ref().expect("find open").query.clone();
        input.update(cx, |input, cx| input.set_value(query.to_string(), window, cx));
        view.run_find_query(cx);
    }

    /// Each match, read back out of the preview's current rendered text.
    fn hits(view: &EditorView, cx: &gpui::App) -> Vec<String> {
        let text = view.md_preview.as_ref().expect("preview").state.read(cx).rendered_text();
        let find = view.find.as_ref().expect("find open");
        find.matches.iter().map(|r| text.as_str()[r.clone()].to_string()).collect()
    }

    /// End to end over a real renderer: find searches what the preview
    /// *shows* (heading markers, `**` and backticks are not in it), steps and
    /// wraps, re-searches when the document re-parses, and is not available
    /// once only the source is on screen.
    #[gpui::test]
    async fn finds_in_the_rendered_preview_and_follows_edits(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plan.md");
        std::fs::write(&path, "# Overview\n\nThe **overview** of `overview`.\n\n- unrelated\n")
            .expect("write");

        let window = cx.add_window(|window, cx| EditorView::new(path, window, cx));
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                assert!(view.open_find(window, cx));
                assert_eq!(view.find.as_ref().unwrap().target, FindTarget::Preview, "a .md opens in Preview");
                search(view, "OVERVIEW", window, cx);
                assert_eq!(hits(view, cx), ["Overview", "overview", "overview"]);
                assert_eq!(view.find.as_ref().unwrap().current, 0);

                view.step_preview_find(-1, cx);
                assert_eq!(view.find.as_ref().unwrap().current, 2, "wraps backwards");
                view.step_preview_find(1, cx);
                assert_eq!(view.find.as_ref().unwrap().current, 0, "wraps forwards");

                // Edit the buffer, the way typing in Split does.
                let super::super::EditorContent::Text(t) = &view.content else { panic!("text buffer") };
                t.state.update(cx, |state, cx| {
                    state.set_value("Intro\n\n# Overview\n\nNothing else.\n", window, cx)
                });
                cx.notify();
            })
            .expect("window alive");
        // Render, which pushes the new text into the renderer. Under 4 KB the
        // renderer parses it synchronously inside that render, where gpui drops
        // the parse's notify — the re-check in `sync_preview_text` is what
        // re-runs the search here (the larger-document test below covers the
        // observer path).
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                assert_eq!(hits(view, cx), ["Overview"], "searched the new document");

                view.md_mode = MarkdownViewMode::Source;
                view.find_after_mode_change(window, cx);
                assert!(view.find.is_none(), "the preview's text left the screen");
                assert!(view.open_find(window, cx));
                assert_eq!(view.find.as_ref().unwrap().target, FindTarget::Source);
            })
            .expect("window alive");
    }

    /// A document over the renderer's 4 KB synchronous-parse limit parses on
    /// the background executor, so the re-search after an edit arrives through
    /// the state observer (the notify lands outside a draw) rather than the
    /// render-time re-check — the path every real plan document takes.
    #[gpui::test]
    async fn a_large_document_re_searches_when_its_background_parse_lands(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let filler: String = (0..200).map(|i| format!("Filler paragraph number {i}.\n\n")).collect();
        assert!(filler.len() > 4 * 1024, "must exceed the synchronous-parse limit");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.md");
        std::fs::write(&path, format!("# Needle\n\n{filler}")).expect("write");

        let window = cx.add_window(|window, cx| EditorView::new(path, window, cx));
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                assert!(view.open_find(window, cx));
                search(view, "needle", window, cx);
                assert_eq!(hits(view, cx), ["Needle"]);

                let super::super::EditorContent::Text(t) = &view.content else { panic!("text buffer") };
                let edited = format!("# Needle\n\n{filler}A second needle.\n");
                t.state.update(cx, |state, cx| state.set_value(edited, window, cx));
                cx.notify();
            })
            .expect("window alive");
        cx.run_until_parked();

        window
            .update(cx, |view, _, cx| {
                assert_eq!(hits(view, cx), ["Needle", "needle"], "the landed parse was searched");
            })
            .expect("window alive");
    }

    #[test]
    fn match_tones_follow_the_theme_and_keep_current_strongest() {
        let amber: Hsla = gpui::rgb(0xD9A441).into();
        let dark = match_tones(amber, true);
        let light = match_tones(amber, false);
        assert_ne!(dark, light, "a theme switch must change the tones, or nothing repaints");
        for tones in [dark, light] {
            assert!(tones.current.a > tones.other.a, "the current match stands out");
            assert_eq!(tones.current.h, amber.h, "one hue for every match");
        }
        // Light text stays readable over the dark theme's current match.
        assert!(dark.current.a < 1.0);
    }

}
