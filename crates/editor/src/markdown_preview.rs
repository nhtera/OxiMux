//! Markdown preview rendering for `EditorView`.
//!
//! `.md`/`.markdown` files get a rendered view in addition to the raw source
//! editor. The actual GFM rendering (code-block syntax highlight, tables,
//! task lists, blockquotes, clickable links, images) is done by the renderer
//! that already ships in `gpui-component` (`text::markdown`) — this module is
//! the thin glue that:
//!
//! 1. Exposes the `MarkdownViewMode` tri-state (Source / Preview / Split) and
//!    the segmented header toggle that drives it.
//! 2. Builds the scrollable preview element.
//! 3. Fixes the one real gap vs. a polished editor: the renderer consumes
//!    image URLs verbatim, so a relative `![](./img.png)` never resolves.
//!    `absolutize_image_paths` rewrites repo-relative image paths to
//!    `file://` URIs against the document's directory before rendering.
//!
//! Keeping this out of `editor_view.rs` holds that file under the size cap and
//! lets the path-rewriting logic be unit-tested as a pure function.

use std::path::{Component, Path, PathBuf};

use gpui::{
    AnyElement, EntityId, InteractiveElement, IntoElement, ParentElement, Styled, div,
    prelude::FluentBuilder as _, px, rems,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, Selectable, Sizable,
    button::{Button, ButtonGroup},
    clipboard::Clipboard,
    h_flex,
    highlighter::HighlightTheme,
    text::{TextView, TextViewStyle},
};

/// Which view a markdown file is showing. `Copy` so it lives as a plain field
/// on `EditorView` and is cheap to compare for the toggle's selected state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarkdownViewMode {
    /// Raw source — the existing code editor, unchanged.
    Source,
    /// Rendered GFM only.
    Preview,
    /// Resizable source-left / preview-right.
    Split,
}

impl MarkdownViewMode {
    /// Map a segmented-toggle button index back to a mode. Order must match
    /// the button order in [`mode_toggle`].
    pub fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Source),
            1 => Some(Self::Preview),
            2 => Some(Self::Split),
            _ => None,
        }
    }
}

/// Build the 3-button segmented toggle reflecting the active `mode`. The
/// caller attaches `.on_click(...)` (it needs an `EditorView` listener to
/// mutate state, which can't be expressed here). Single-select is the
/// `ButtonGroup` default, so exactly one button reads as selected.
///
/// Icon-only with hover tooltips — a compact segmented control that reads as
/// chrome rather than competing with the document. `view_id` scopes the
/// element id to the owning view so two open `.md` tabs never share state.
pub fn mode_toggle(mode: MarkdownViewMode, view_id: EntityId) -> ButtonGroup {
    ButtonGroup::new(("md-mode", view_id))
        .compact()
        .xsmall()
        .child(
            Button::new("md-mode-source")
                .icon(Icon::empty().path("icons/code.svg"))
                .tooltip("Source")
                .selected(mode == MarkdownViewMode::Source),
        )
        .child(
            Button::new("md-mode-preview")
                .icon(IconName::Eye)
                .tooltip("Preview")
                .selected(mode == MarkdownViewMode::Preview),
        )
        .child(
            Button::new("md-mode-split")
                .icon(Icon::empty().path("icons/columns.svg"))
                .tooltip("Split")
                .selected(mode == MarkdownViewMode::Split),
        )
}

/// Styling for the rendered preview. The renderer's own default is a *light*
/// code highlight theme, so the syntax set and surface must follow the active
/// app theme (`is_dark`) or code blocks read washed-out under dark and
/// over-bright under light. Also opens up the paragraph rhythm a touch so long
/// docs breathe.
fn preview_style(is_dark: bool) -> TextViewStyle {
    let highlight_theme = if is_dark {
        HighlightTheme::default_dark()
    } else {
        HighlightTheme::default_light()
    };
    TextViewStyle {
        is_dark,
        highlight_theme,
        ..Default::default()
    }
    .paragraph_gap(rems(1.1))
}

/// Render the markdown preview element: absolutize relative image paths, then
/// hand the source to the GFM renderer. Wrapped in a bounded `flex_1`
/// container because `scrollable(true)` virtualizes via `gpui::list`, which
/// needs a fixed-height parent (see `TextView::scrollable` docs).
///
/// `base_dir` is the document's directory (`file_path.parent()`); when `None`
/// (file at filesystem root, no parent) image paths are left untouched.
///
/// `view_id` scopes the element ids (wrapper + `TextView`) to the owning view
/// so two open `.md` tabs never share the renderer's keyed state. `is_dark`
/// follows the active app theme so preview surface + code-block syntax track
/// dark/light.
pub fn render_preview(
    source: &str,
    base_dir: Option<&Path>,
    view_id: EntityId,
    is_dark: bool,
) -> AnyElement {
    let rendered = match base_dir {
        Some(dir) => absolutize_image_paths(source, dir),
        None => source.to_owned(),
    };
    div()
        .id(("md-preview", view_id))
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .child(
            TextView::markdown(("md-preview-text", view_id), rendered)
                .style(preview_style(is_dark))
                // Code blocks get a language tag + one-click copy, the way a
                // polished doc viewer surfaces fenced code.
                .code_block_actions(|code_block, _window, cx| {
                    let code = code_block.code();
                    h_flex()
                        .gap_2()
                        .items_center()
                        .when_some(code_block.lang(), |this, lang| {
                            this.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(lang),
                            )
                        })
                        .child(Clipboard::new("md-code-copy").value(code))
                })
                .h_full()
                .p_5()
                .scrollable(true)
                .selectable(true),
        )
        .into_any_element()
}

/// Rewrite repo-relative image paths in `![alt](url)` / `![alt](url "title")`
/// to `file://` URIs resolved against `base_dir`. Absolute paths, `http(s)://`,
/// `file://`, `data:`, protocol-relative `//`, any `scheme://`, and `#anchor`
/// targets are left untouched. Pure so it is unit-testable without a renderer.
pub(crate) fn absolutize_image_paths(src: &str, base_dir: &Path) -> String {
    let bytes = src.as_bytes();
    // Output as bytes: every byte we branch on (`!`, `[`, `]`, `(`, `)`, `"`,
    // `'`) is ASCII and so never sits inside a multi-byte UTF-8 sequence —
    // copying non-matching bytes one at a time preserves valid UTF-8.
    let mut out: Vec<u8> = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'!'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'['
            && let Some(bracket) = find_link_open(src, i + 2)
            && let Some(close) = find_paren_close(src, bracket + 2)
        {
            let alt = &src[i + 2..bracket];
            let inner = &src[bracket + 2..close];
            let (url, title) = split_url_title(inner);
            if let Some(abs) = absolutize_one(url, base_dir) {
                out.extend_from_slice(b"![");
                out.extend_from_slice(alt.as_bytes());
                out.extend_from_slice(b"](");
                out.extend_from_slice(abs.as_bytes());
                if let Some(title) = title {
                    out.push(b' ');
                    out.extend_from_slice(title.as_bytes());
                }
                out.push(b')');
                i = close + 1;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Safe: input was valid UTF-8 and every inserted byte sequence is either
    // an ASCII literal or a (UTF-8) substring of the input / a percent-encoded
    // ASCII file:// URL.
    String::from_utf8(out).unwrap_or_else(|_| src.to_owned())
}

/// From the index just past `![`, find the `](` that closes the alt text.
/// Returns the index of `]`. Uses the first `](` — nested `]` in alt text is
/// vanishingly rare in image syntax and not worth a full bracket-matcher.
fn find_link_open(src: &str, from: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            return Some(i);
        }
        // Bail at a newline: an image's `](` is on the same line as `![`.
        if bytes[i] == b'\n' {
            return None;
        }
        i += 1;
    }
    None
}

/// From the index just past `](`, find the matching `)`, skipping any `)` that
/// sits inside a quoted title (`"..."` or `'...'`). Returns the `)` index.
fn find_paren_close(src: &str, from: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = from;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b')' => return Some(i),
                b'\n' => return None,
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// Split the inside of `(...)` into the URL and an optional trailing title
/// (kept verbatim, quotes included). Handles `<url>` angle-bracket wrapping.
fn split_url_title(inner: &str) -> (&str, Option<&str>) {
    let trimmed = inner.trim();
    if let Some(rest) = trimmed.strip_prefix('<')
        && let Some(end) = rest.find('>')
    {
        let url = &rest[..end];
        let title = rest[end + 1..].trim();
        return (url, (!title.is_empty()).then_some(title));
    }
    match trimmed.find(char::is_whitespace) {
        Some(sp) => {
            let url = &trimmed[..sp];
            let title = trimmed[sp..].trim();
            (url, (!title.is_empty()).then_some(title))
        }
        None => (trimmed, None),
    }
}

/// If `url` is a repo-relative path, resolve it against `base_dir` and return
/// a `file://` URI string; otherwise return `None` (leave the URL as-is).
fn absolutize_one(url: &str, base_dir: &Path) -> Option<String> {
    let url = url.trim();
    if url.is_empty()
        || url.starts_with('#')
        || url.starts_with('/') // absolute path or protocol-relative `//`
        || url.starts_with("data:")
        || url.starts_with("mailto:")
        || url.contains("://")
    {
        return None;
    }
    let joined = normalize_lexically(&base_dir.join(url));
    url::Url::from_file_path(&joined)
        .ok()
        .map(|u| u.to_string())
}

/// Lexically resolve `.` / `..` components without touching the filesystem so
/// the produced `file://` URL is clean (and the function stays pure/testable).
///
/// `..` past the root is clamped by `PathBuf::pop` (a no-op at root), so an
/// over-deep `../../x.png` resolves to `/x.png`. That file simply won't exist
/// and the image renders broken — acceptable, and the renderer can only
/// display a file, never read it back out, so there is no escape concern.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The document directory these fixtures resolve against, absolute for the
    /// running platform.
    ///
    /// `/repo/docs` is not absolute on Windows, and `absolutize_one` ends in
    /// `url::Url::from_file_path`, which requires an absolute path — so every
    /// rewrite declined and the URLs came back unchanged. The production path is
    /// portable precisely *because* it delegates to the `url` crate instead of
    /// formatting `file://` by hand: a real `C:\repo\docs` yields
    /// `file:///C:/repo/docs/cat.png`, drive letter and separators handled. Only
    /// the fixture was Unix-only.
    fn base() -> &'static Path {
        if cfg!(windows) {
            Path::new(r"C:\repo\docs")
        } else {
            Path::new("/repo/docs")
        }
    }

    fn abs(src: &str) -> String {
        absolutize_image_paths(src, base())
    }

    /// The `file://` URL `base()` resolves `rel` to.
    ///
    /// Most tests here are about the *scanner* — finding `![…](…)`, skipping
    /// titles and angle brackets, leaving remote URLs alone — and the URL text is
    /// incidental to that, so deriving it keeps them from carrying a
    /// per-platform expected string apiece. The URL *shape* is pinned literally
    /// in `rewrites_simple_relative_image` and the percent-encoding in
    /// `rewrites_angle_bracket_url`, so an encoding change still fails somewhere.
    fn url_for(rel: &str) -> String {
        url::Url::from_file_path(normalize_lexically(&base().join(rel)))
            .expect("fixture base is absolute")
            .to_string()
    }

    #[test]
    fn rewrites_simple_relative_image() {
        // The one literal expectation, so the URL shape itself is pinned rather
        // than derived from the same call the code makes.
        let expected = if cfg!(windows) {
            "![cat](file:///C:/repo/docs/cat.png)"
        } else {
            "![cat](file:///repo/docs/cat.png)"
        };
        assert_eq!(abs("![cat](cat.png)"), expected);
    }

    #[test]
    fn rewrites_dot_slash_relative_image() {
        assert_eq!(
            abs("![x](./img/cat.png)"),
            format!("![x]({})", url_for("img/cat.png"))
        );
    }

    #[test]
    fn resolves_parent_dir_segments() {
        assert_eq!(
            abs("![x](../assets/cat.png)"),
            format!("![x]({})", url_for("../assets/cat.png"))
        );
    }

    #[test]
    fn preserves_title_after_url() {
        assert_eq!(
            abs(r#"![x](cat.png "a cat")"#),
            format!(r#"![x]({} "a cat")"#, url_for("cat.png"))
        );
    }

    #[test]
    fn leaves_remote_and_absolute_untouched() {
        for src in [
            "![x](https://example.com/cat.png)",
            "![x](http://example.com/cat.png)",
            "![x](/abs/cat.png)",
            "![x](file:///already/abs.png)",
            "![x](data:image/png;base64,AAAA)",
            "![x](//cdn.example.com/cat.png)",
        ] {
            assert_eq!(abs(src), src, "must not rewrite: {src}");
        }
    }

    #[test]
    fn parent_dir_past_root_clamps_to_root() {
        // `..` is clamped at root rather than producing `/../`; the file just
        // won't exist (broken image), which is the accepted degraded outcome.
        //
        // "Root" is platform-shaped: `/` on unix, but the drive root `C:\` on
        // Windows, where clamping stops at the prefix rather than discarding it.
        let (root, expected) = if cfg!(windows) {
            (Path::new(r"C:\repo"), "![x](file:///C:/x.png)")
        } else {
            (Path::new("/repo"), "![x](file:///x.png)")
        };
        assert_eq!(
            absolutize_image_paths("![x](../../../x.png)", root),
            expected
        );
    }

    #[test]
    fn unterminated_title_leaves_text_unchanged() {
        // No closing `)` (quote never closes) → scanner bails, text untouched.
        let src = "![x](img.png \"unterminated";
        assert_eq!(abs(src), src);
    }

    #[test]
    fn rewrites_angle_bracket_url() {
        // Literal, because percent-encoding the space is the point of this one.
        let expected = if cfg!(windows) {
            "![x](file:///C:/repo/docs/my%20cat.png)"
        } else {
            "![x](file:///repo/docs/my%20cat.png)"
        };
        assert_eq!(abs("![x](<my cat.png>)"), expected);
    }

    #[test]
    fn handles_multiple_images_and_surrounding_text() {
        let src = "intro ![a](a.png) middle ![b](https://x/y.png) end ![c](sub/c.png)";
        let got = abs(src);
        assert!(got.contains(&format!("![a]({})", url_for("a.png"))), "{got}");
        assert!(got.contains("![b](https://x/y.png)"), "{got}");
        assert!(
            got.contains(&format!("![c]({})", url_for("sub/c.png"))),
            "{got}"
        );
    }

    #[test]
    fn leaves_non_image_text_unchanged() {
        let src = "# Heading\n\nA [link](page.md) is not an image.\n\n```rust\nfn main() {}\n```\n";
        assert_eq!(abs(src), src);
    }

    #[test]
    fn preserves_utf8_content() {
        let src = "日本語 ![猫](neko.png) テキスト";
        assert_eq!(
            abs(src),
            format!("日本語 ![猫]({}) テキスト", url_for("neko.png"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_absolute_image_path_resolves_rather_than_being_joined() {
        // The Windows analogue of the `/abs/cat.png` case in
        // `leaves_remote_and_absolute_untouched`. `C:/abs/cat.png` trips none of
        // the early-outs — no leading `/`, no `://` — so it reaches
        // `base_dir.join(url)`, and `Path::join` *replaces* the base when the
        // argument is absolute. The result is the drive path itself, not
        // `C:\repo\docs\C:\abs\...`, which is the correct answer arrived at by a
        // route worth pinning: an early-out added later must not change it.
        assert_eq!(
            abs("![x](C:/abs/cat.png)"),
            "![x](file:///C:/abs/cat.png)"
        );
    }

    #[test]
    fn from_index_maps_button_order() {
        assert_eq!(MarkdownViewMode::from_index(0), Some(MarkdownViewMode::Source));
        assert_eq!(MarkdownViewMode::from_index(1), Some(MarkdownViewMode::Preview));
        assert_eq!(MarkdownViewMode::from_index(2), Some(MarkdownViewMode::Split));
        assert_eq!(MarkdownViewMode::from_index(3), None);
    }
}
