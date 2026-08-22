//! Keep every token-caching view pulling the current appearance.
//!
//! Views are handed a `Density` and a `Typography` when they are built and
//! keep them. That is deliberate — it keeps render paths total and testable —
//! but it means a live density or zoom change leaves every one of those
//! snapshots stale. The refresh is therefore a pull: each `Render` impl calls
//! `oximux_settings::appearance::sync` at the top of its `render`, and cannot
//! then be stale for longer than a frame.
//!
//! The failure mode this exists to stop is the quiet one. A new view that
//! caches tokens and forgets the call compiles, renders, passes its tests, and
//! looks perfect — right up until someone changes the preference, at which
//! point that one pane stays behind at the old size while everything around it
//! moves. Nothing errors, and the reviewer of the *next* diff has no reason to
//! look at it.
//!
//! Unlike the file-size and literal ratchets this has no allowlist: the set is
//! small, the fix is one line, and a view that opts out is exactly the bug.

use std::collections::HashMap;
use std::error::Error;

/// The call that satisfies the lint. Matched on the module-qualified suffix so
/// it holds whether the site writes the full path or imports the module.
const SYNC_CALLS: &[&str] = &["appearance::sync(", "appearance::sync_typography("];

/// The half-answer: it sizes the type scale and leaves the faces at the
/// platform default, so a caller that stops there paints its surface in a
/// typeface the user replaced while everything around it obeys.
/// `oximux_settings::appearance::typography(cx)` is the whole answer.
const HALF_RESOLVER: &str = "Typography::for_appearance(";
/// What to reach for instead.
const WHOLE_RESOLVER: &str = "appearance::typography(cx)";

/// One view that caches tokens without refreshing them.
pub struct Miss {
    pub view: String,
    pub file: String,
}

/// One site that builds a type scale without the user's font choice.
pub struct HalfResolved {
    pub file: String,
    pub line: usize,
}

/// The portion of `text` that ships — everything before the first
/// `#[cfg(test)]`.
///
/// Test code legitimately builds a scale from a hand-made `Appearance` with no
/// `App` to ask for the font choice, and demanding the whole resolver there
/// would mean standing up a GPUI context to assert on a padding.
fn shipped(text: &str) -> &str {
    match text.find("#[cfg(test)]") {
        Some(at) => &text[..at],
        None => text,
    }
}

/// Every call to the half-answer in code that ships.
fn half_resolved(text: &str) -> Vec<usize> {
    let body = shipped(text);
    let mut lines = Vec::new();
    for (at, _) in body.match_indices(HALF_RESOLVER) {
        let line_start = body[..at].rfind('\n').map_or(0, |n| n + 1);
        // A mention in a comment is documentation, not a call — this lint's
        // own explanation of what to use instead names it.
        if body[line_start..at].contains("//") {
            continue;
        }
        lines.push(body[..at].matches('\n').count() + 1);
    }
    lines
}

/// The body between the first `{` at or after `from` and its matching `}`.
fn balanced(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let open = text[from..].find('{')? + from;
    let mut depth = 0usize;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, i));
                }
            }
            _ => {}
        }
    }
    None
}

/// Every `NAME` in `text` whose `struct NAME { … }` body declares a cached
/// `density` or `typography`.
fn token_holders(text: &str, into: &mut HashMap<String, ()>) {
    for (at, _) in text.match_indices("struct ") {
        // Skip a mention inside a line comment.
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        if text[line_start..at].contains("//") {
            continue;
        }
        let after = at + "struct ".len();
        let name: String = text[after..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let Some((open, close)) = balanced(text, after) else {
            continue;
        };
        let body = &text[open..close];
        if body.contains("density: Density") || body.contains("typography: Typography") {
            into.insert(name, ());
        }
    }
}

/// Token-caching `Render` impls in `text` whose `render` never syncs.
fn unsynced(text: &str, holders: &HashMap<String, ()>) -> Vec<String> {
    let mut misses = Vec::new();
    for (at, _) in text.match_indices("impl Render for ") {
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        if text[line_start..at].contains("//") {
            continue;
        }
        let after = at + "impl Render for ".len();
        let name: String = text[after..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !holders.contains_key(&name) {
            continue;
        }
        let Some((open, close)) = balanced(text, after) else {
            continue;
        };
        let impl_body = &text[open..close];
        let Some(fn_at) = impl_body.find("fn render") else {
            continue;
        };
        let Some((body_open, body_close)) = balanced(impl_body, fn_at) else {
            continue;
        };
        let render_body = &impl_body[body_open..body_close];
        if !SYNC_CALLS.iter().any(|call| render_body.contains(call)) {
            misses.push(name);
        }
    }
    misses
}

/// Scan `files` (path, text) and return every view that caches tokens without
/// pulling the current appearance.
pub fn scan(files: &[(String, String)]) -> Vec<Miss> {
    let mut holders = HashMap::new();
    for (_, text) in files {
        token_holders(text, &mut holders);
    }
    let mut misses = Vec::new();
    for (path, text) in files {
        for view in unsynced(text, &holders) {
            misses.push(Miss {
                view,
                file: path.clone(),
            });
        }
    }
    misses
}

/// Scan `files` for sites that size the type scale without the face choice.
///
/// The settings crate is exempt: it is where the whole resolver is written, and
/// it has to call the half-answer to build one.
pub fn scan_resolvers(files: &[(String, String)]) -> Vec<HalfResolved> {
    let mut hits = Vec::new();
    for (path, text) in files {
        if path.replace('\\', "/").contains("crates/settings/") {
            continue;
        }
        for line in half_resolved(text) {
            hits.push(HalfResolved {
                file: path.clone(),
                line,
            });
        }
    }
    hits
}

pub fn run(files: &[(String, String)]) -> Result<(), Box<dyn Error>> {
    let misses = scan(files);
    let half = scan_resolvers(files);
    if misses.is_empty() && half.is_empty() {
        println!(
            "appearance-lint: every token-caching view pulls the current appearance, \
             and every type scale is resolved with the chosen faces"
        );
        return Ok(());
    }
    for miss in &misses {
        eprintln!(
            "{}: `{}` caches density/typography but its render never calls \
             oximux_settings::appearance::sync",
            miss.file, miss.view
        );
    }
    for hit in &half {
        eprintln!(
            "{}:{}: `{HALF_RESOLVER}…)` leaves the font choice behind — use \
             `oximux_settings::{WHOLE_RESOLVER}`",
            hit.file, hit.line
        );
    }
    let mut reasons = Vec::new();
    if !misses.is_empty() {
        reasons.push(format!(
            "{} view(s) would go stale on a density or zoom change",
            misses.len()
        ));
    }
    if !half.is_empty() {
        reasons.push(format!(
            "{} site(s) would ignore the chosen font",
            half.len()
        ));
    }
    Err(format!("appearance-lint: {}", reasons.join("; ")).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(src: &str) -> Vec<(String, String)> {
        vec![("test.rs".to_string(), src.to_string())]
    }

    #[test]
    fn a_caching_view_that_syncs_passes() {
        let src = "\
pub struct Panel { density: Density, typography: Typography }
impl Render for Panel {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.density, &mut self.typography, cx);
        div()
    }
}
";
        assert!(scan(&files(src)).is_empty());
    }

    #[test]
    fn a_caching_view_that_forgets_is_caught() {
        // The whole point: this compiles and renders correctly today, and is
        // wrong only after someone changes a preference.
        let src = "\
pub struct Panel { density: Density, typography: Typography }
impl Render for Panel {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
";
        let misses = scan(&files(src));
        assert_eq!(misses.len(), 1);
        assert_eq!(misses[0].view, "Panel");
    }

    #[test]
    fn the_typography_only_variant_counts_as_a_sync() {
        let src = "\
pub struct Panel { typography: Typography }
impl Render for Panel {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync_typography(&mut self.typography, cx);
        div()
    }
}
";
        assert!(scan(&files(src)).is_empty());
    }

    #[test]
    fn a_view_that_caches_nothing_is_not_asked_to_sync() {
        // Views that take their tokens as arguments have nothing to go stale,
        // and demanding the call from them would be noise.
        let src = "\
pub struct Ghost { width: f32 }
impl Render for Ghost {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
";
        assert!(scan(&files(src)).is_empty());
    }

    #[test]
    fn the_struct_and_the_impl_may_live_in_different_files() {
        // The common shape in this tree: fields in `mod.rs`, `Render` in
        // `render.rs`. A per-file check would miss every one of them.
        let decl = ("mod.rs".to_string(),
            "pub struct Panel { density: Density, typography: Typography }".to_string());
        let imp = ("render.rs".to_string(), "\
impl Render for Panel {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
".to_string());
        let misses = scan(&[decl, imp]);
        assert_eq!(misses.len(), 1);
        assert_eq!(misses[0].file, "render.rs");
    }

    #[test]
    fn a_mention_in_a_comment_is_not_an_impl() {
        // `paint/mod.rs` names `impl Render for DiffView` in a comment that
        // explains where the real one moved to.
        let src = "\
pub struct Panel { density: Density, typography: Typography }
// `impl Render for Panel` lives in render.rs
";
        assert!(scan(&files(src)).is_empty());
    }

    #[test]
    fn resolving_a_type_scale_without_the_font_choice_is_caught() {
        // The failure this stops is invisible until someone picks a font: the
        // surface sizes correctly, syncs correctly, and is drawn in a typeface
        // the user replaced everywhere else.
        let files = vec![(
            "apps/desktop/src/shell/rail.rs".to_string(),
            "let typography = Typography::for_appearance(appearance);".to_string(),
        )];
        let hits = scan_resolvers(&files);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 1);
    }

    #[test]
    fn the_crate_that_defines_the_whole_resolver_may_call_the_half_one() {
        let files = vec![(
            "crates/settings/src/appearance.rs".to_string(),
            "Typography::for_appearance(active(cx)).with_fonts(fonts)".to_string(),
        )];
        assert!(scan_resolvers(&files).is_empty());
    }

    #[test]
    fn a_test_helper_may_resolve_from_a_hand_made_appearance() {
        // Unit tests build an `Appearance` directly and have no `App` to ask
        // for the font choice; requiring the whole resolver there would mean
        // standing up a GPUI context to assert on a padding.
        let files = vec![(
            "apps/desktop/src/shell/style.rs".to_string(),
            "pub fn new() {}\n#[cfg(test)]\nmod tests {\n  Typography::for_appearance(a);\n}"
                .to_string(),
        )];
        assert!(scan_resolvers(&files).is_empty());
    }

    #[test]
    fn a_mention_in_a_comment_is_not_a_call() {
        let files = vec![(
            "apps/desktop/src/shell/rail.rs".to_string(),
            "// Typography::for_appearance( is the half-answer; do not use it.".to_string(),
        )];
        assert!(scan_resolvers(&files).is_empty());
    }

    #[test]
    fn a_helper_named_render_does_not_satisfy_the_impl() {
        // The sync has to be in the `Render::render` body. A sibling helper
        // that happens to call it does not make the view fresh.
        let src = "\
pub struct Panel { density: Density, typography: Typography }
impl Panel {
    fn render_row(&self) { oximux_settings::appearance::sync(a, b, cx); }
}
impl Render for Panel {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
";
        assert_eq!(scan(&files(src)).len(), 1);
    }
}
