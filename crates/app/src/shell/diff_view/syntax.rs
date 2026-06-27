//! Syntect-driven syntax highlighting for the diff body.
//!
//! Per-line tokenizer. Each call to [`highlight_line`] returns a list of
//! [`HiToken`]s — each carries its byte range inside the line plus the
//! foreground color sintect picked from the active theme. The renderer
//! paints one `div().text_color()` per token.
//!
//! ### Why syntect (and not tree-sitter)
//!
//! - One crate, one API. Tree-sitter would mean a parser crate per language
//!   plus `tree-sitter-highlight` plus per-language `highlights.scm`.
//! - The bundled Sublime Text 2 grammars cover the languages we care about
//!   (Rust, TypeScript, JavaScript, Markdown, TOML, JSON) and ship inside
//!   the syntect crate — no extra grammar crates, no `build.rs` C compiles.
//! - `default-fancy` selects the pure-Rust `fancy-regex` engine instead of
//!   the C `onig` binding. No `.a` link on macOS, ~300 KB smaller binary.
//! - Per-line restart is built in via `HighlightLines` — easy to feed one
//!   diff row at a time without re-parsing the file.
//!
//! ### Threading + caching
//!
//! The `SyntaxSet` is loaded once via `LazyLock` on first call (~30 ms cold).
//! Subsequent calls reuse it. A second `LazyLock` holds the active theme.
//! Both are `Send + Sync` so they're safe to read from the GPUI thread.

use std::io::Cursor;
use std::path::Path;
use std::sync::LazyLock;

#[cfg(test)]
use syntect::highlighting::Style;
use syntect::highlighting::{HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

/// Lazy holders. `SyntaxSet::load_defaults_nonewlines()` is the heavy call
/// (~30 ms cold start, ~550 KB embedded grammars). Read-only after init.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_nonewlines);

/// The diff syntax theme — a conventional dark-editor token palette
/// (keyword-blue / string-orange / comment-green) bundled as a TextMate
/// theme (`assets/themes/syntax-dark.tmTheme`) and embedded at compile
/// time. Only token foregrounds are consumed; the theme's background is
/// inert (OxiMux paints its own charcoal surface).
static SYNTAX_DARK: LazyLock<Theme> = LazyLock::new(|| {
    let bytes = include_bytes!("../../../assets/themes/syntax-dark.tmTheme");
    ThemeSet::load_from_reader(&mut Cursor::new(&bytes[..]))
        .expect("bundled syntax-dark.tmTheme parses")
});

/// The active highlight theme for the diff body.
fn active_theme() -> &'static Theme {
    &SYNTAX_DARK
}

/// The syntect highlighter, derived once from the bundled theme. Building it
/// walks every scope selector in the theme to build its match caches, so it
/// must NOT be reconstructed per line. The original code created a fresh
/// `HighlightLines` (which rebuilds this) on every `highlight_line` call —
/// ~6 ms/line in a debug build, i.e. a ~1 s stall to highlight a 170-line
/// diff. Shared here so each line pays only a cheap `ParseState` +
/// `HighlightState` setup.
static HIGHLIGHTER: LazyLock<Highlighter<'static>> =
    LazyLock::new(|| Highlighter::new(active_theme()));

/// Coarse language buckets — keyed on file extension. Mirrors the
/// languages we ship grammars for; everything else degrades to `Unknown`
/// and skips highlighting entirely.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Markdown,
    Toml,
    Json,
    Unknown,
}

/// Map a file path to its language bucket. Extension-only lookup — no
/// shebang sniffing, no content heuristics. Extension-less files (e.g.
/// `Dockerfile`, `Makefile`) fall through to `Unknown`.
pub fn detect_language(path: &Path) -> Language {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("rs") => Language::Rust,
        Some("ts") | Some("tsx") => Language::TypeScript,
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => Language::JavaScript,
        Some("md") | Some("markdown") => Language::Markdown,
        Some("toml") => Language::Toml,
        Some("json") | Some("jsonc") => Language::Json,
        _ => Language::Unknown,
    }
}

/// One byte-range slice of a line plus the sRGB foreground color syntect
/// picked for it. Renderer uses `r/g/b` directly via `gpui::Rgba`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiToken {
    pub start: usize,
    pub end: usize,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Tokenize one line of source text under the given language. Returns
/// an empty vec when:
/// - the line is empty (nothing to render colored),
/// - the language is `Unknown` (no grammar to drive highlighting), OR
/// - syntect's `highlight_line` fails (shouldn't happen on valid UTF-8 —
///   we log nothing because a syntax-highlight failure should never break
///   diff rendering).
///
/// The caller (`paint.rs`) treats empty output as "fall back to muted base
/// color", which keeps the renderer simple and prevents a bad grammar
/// match from blanking out the row.
pub fn highlight_line(line: &str, lang: Language) -> Vec<HiToken> {
    if line.is_empty() || matches!(lang, Language::Unknown) {
        return Vec::new();
    }
    let Some(syntax) = syntax_for(lang) else {
        return Vec::new();
    };
    // Each diff row is highlighted standalone — a fresh parse + highlight
    // state per line — preserving the prior per-line behaviour (added/removed
    // rows must not leak multi-line string/comment state across the +/-
    // boundary). Only the heavy `Highlighter` (theme scope cache) is shared.
    let highlighter = &*HIGHLIGHTER;
    let mut parse_state = ParseState::new(syntax);
    let mut highlight_state = HighlightState::new(highlighter, ScopeStack::new());
    // syntect expects the trailing `\n` so it knows the line ended. Diff rows
    // don't carry it (`parse_hunk_body_line` strips it), so re-add it on a
    // cheap single-line allocation.
    let mut padded = String::with_capacity(line.len() + 1);
    padded.push_str(line);
    padded.push('\n');
    let Ok(ops) = parse_state.parse_line(&padded, &SYNTAX_SET) else {
        return Vec::new();
    };
    // Translate syntect's (Style, &str) pairs into our HiToken with explicit
    // byte offsets relative to the *original* line content. The offset tracks
    // cumulative chunk len — syntect guarantees the chunks concatenate back to
    // the input, in order, including the trailing newline char.
    let mut out = Vec::new();
    let mut offset = 0usize;
    for (style, chunk) in HighlightIterator::new(&mut highlight_state, &ops, &padded, highlighter) {
        let len = chunk.len();
        // Drop the trailing-newline token — it lives outside the diff-row's
        // content slice.
        let end = (offset + len).min(line.len());
        if offset < line.len() {
            out.push(HiToken {
                start: offset,
                end,
                r: style.foreground.r,
                g: style.foreground.g,
                b: style.foreground.b,
            });
        }
        offset += len;
    }
    out
}

/// Look up the syntect syntax reference for our language bucket. Falls
/// back to plain-text when the bundled set doesn't carry the language —
/// in practice every variant above is covered, but the fallback keeps
/// the function total.
fn syntax_for(lang: Language) -> Option<&'static SyntaxReference> {
    let token = match lang {
        Language::Rust => "rs",
        Language::TypeScript => "ts",
        Language::JavaScript => "js",
        Language::Markdown => "md",
        Language::Toml => "toml",
        Language::Json => "json",
        Language::Unknown => return None,
    };
    SYNTAX_SET
        .find_syntax_by_extension(token)
        .or_else(|| Some(SYNTAX_SET.find_syntax_plain_text()))
}

/// Pre-warm the static sets. Called from a background task at app boot
/// (`main.rs`) so the ~30 ms cold-start never lands on the first diff
/// paint. Safe to call any number of times; first call to
/// `highlight_line` would warm them anyway.
pub fn prewarm() {
    let _ = (&*SYNTAX_SET, &*SYNTAX_DARK);
}

/// Test-only — expose a Style→u8 helper. Production code reads
/// `HiToken.r/g/b` directly.
#[cfg(test)]
fn style_color(s: Style) -> (u8, u8, u8) {
    (s.foreground.r, s.foreground.g, s.foreground.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_language_known_extensions() {
        assert_eq!(
            detect_language(&PathBuf::from("src/main.rs")),
            Language::Rust
        );
        assert_eq!(
            detect_language(&PathBuf::from("App.tsx")),
            Language::TypeScript
        );
        assert_eq!(
            detect_language(&PathBuf::from("README.md")),
            Language::Markdown
        );
        assert_eq!(
            detect_language(&PathBuf::from("Cargo.toml")),
            Language::Toml
        );
        assert_eq!(
            detect_language(&PathBuf::from("package.json")),
            Language::Json
        );
    }

    #[test]
    fn detect_language_case_insensitive() {
        assert_eq!(
            detect_language(&PathBuf::from("README.MD")),
            Language::Markdown
        );
        assert_eq!(detect_language(&PathBuf::from("Foo.RS")), Language::Rust);
    }

    #[test]
    fn detect_language_unknown_for_extensionless() {
        assert_eq!(
            detect_language(&PathBuf::from("Dockerfile")),
            Language::Unknown
        );
        assert_eq!(
            detect_language(&PathBuf::from("Makefile")),
            Language::Unknown
        );
    }

    #[test]
    fn highlight_empty_line_yields_no_tokens() {
        assert!(highlight_line("", Language::Rust).is_empty());
    }

    #[test]
    fn highlight_unknown_language_yields_no_tokens() {
        assert!(highlight_line("anything goes", Language::Unknown).is_empty());
    }

    #[test]
    fn highlight_rust_keyword_and_string_get_different_colors() {
        // `let` (keyword) and `"hello"` (string) should land on different
        // foreground colors under any sane theme. The exact values are
        // theme-dependent so we assert on inequality only.
        let toks = highlight_line(r#"let x = "hello";"#, Language::Rust);
        assert!(!toks.is_empty(), "rust line should produce tokens");
        // Find a token covering `let` and one covering the string body.
        let let_tok = toks.iter().find(|t| t.start == 0).expect("token at start");
        let string_tok = toks.iter().find(|t| t.start >= 8 && t.end <= 16);
        if let Some(s) = string_tok {
            assert_ne!(
                (let_tok.r, let_tok.g, let_tok.b),
                (s.r, s.g, s.b),
                "keyword and string should have distinct colors"
            );
        }
    }

    #[test]
    fn syntax_theme_keyword_and_string_have_expected_hues() {
        // Lock the dark-editor hues: keyword #569CD6 (blue), string body
        // #CE9178 (orange). Guards against an accidental theme swap silently
        // regressing the syntax palette.
        let toks = highlight_line(r#"let x = "hello";"#, Language::Rust);
        let kw = toks.iter().find(|t| t.start == 0).expect("keyword token");
        assert_eq!((kw.r, kw.g, kw.b), (0x56, 0x9C, 0xD6), "keyword should be blue");
        // The string body sits inside the quotes (bytes 8..=15).
        let s = toks
            .iter()
            .find(|t| t.start >= 9 && t.end <= 15)
            .expect("string token");
        assert_eq!((s.r, s.g, s.b), (0xCE, 0x91, 0x78), "string should be orange");
    }

    #[test]
    fn highlight_tokens_cover_whole_line() {
        // Concatenating the token byte ranges should reconstruct the line
        // (modulo gaps for trailing newline, which we drop). At minimum,
        // the last token's end should be <= line.len(), and the first
        // should start at 0.
        let line = "let x = 1;";
        let toks = highlight_line(line, Language::Rust);
        assert!(toks.first().is_some_and(|t| t.start == 0));
        assert!(toks.last().is_some_and(|t| t.end == line.len()));
        // No gaps and no overlaps.
        for w in toks.windows(2) {
            assert_eq!(
                w[0].end, w[1].start,
                "token ranges should not gap or overlap"
            );
        }
    }

    #[test]
    fn prewarm_is_idempotent() {
        // Should be safe to call multiple times. The LazyLock guarantees
        // single init; this test just exercises the entry point.
        prewarm();
        prewarm();
    }

    #[test]
    fn style_color_helper_returns_rgb() {
        let s = Style::default();
        let (r, g, b) = style_color(s);
        // Default foreground is non-zero in any sane theme but this is
        // a Style::default() so the values are theme-independent zeroes.
        assert_eq!((r, g, b), (0, 0, 0));
    }
}
