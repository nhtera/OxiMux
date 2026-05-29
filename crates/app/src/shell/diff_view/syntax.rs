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

use std::path::Path;
use std::sync::LazyLock;

use syntect::easy::HighlightLines;
#[cfg(test)]
use syntect::highlighting::Style;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Lazy holders. `SyntaxSet::load_defaults_nonewlines()` is the heavy call
/// (~30 ms cold start, ~550 KB embedded grammars). `ThemeSet::load_defaults`
/// is cheap by comparison. Both are read-only after init.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_nonewlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Pick the theme that best matches OxiMux's dark cockpit palette. The
/// bundled `base16-ocean.dark` reads well against `bg_panel` and has clear
/// keyword/string/comment differentiation. Light-theme support is a v1.1
/// follow-up — switching themes means re-running the highlighter, which
/// is cheap, but the renderer's row-tint alphas were tuned against dark.
fn active_theme() -> &'static Theme {
    &THEME_SET.themes["base16-ocean.dark"]
}

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
    let mut hl = HighlightLines::new(syntax, active_theme());
    // syntect expects each call to receive lines *with* their trailing `\n`
    // — that's how it knows the line ended. Diff rows don't carry the
    // newline (`parse_hunk_body_line` strips it), so we feed
    // `LinesWithEndings::from(...)` over a `String` containing the line
    // plus a fresh `\n`. Cheap allocation, single line each time.
    let mut padded = String::with_capacity(line.len() + 1);
    padded.push_str(line);
    padded.push('\n');
    let mut out = Vec::new();
    for ln in LinesWithEndings::from(&padded) {
        let Ok(ranges) = hl.highlight_line(ln, &SYNTAX_SET) else {
            return Vec::new();
        };
        // Translate syntect's (Style, &str) pairs into our HiToken with
        // explicit byte offsets relative to the *original* line content.
        // We compute the offset by tracking cumulative len of the &str
        // chunks — syntect guarantees the chunks concatenate back to
        // the input line, in order, including any trailing newline char.
        let mut offset = 0usize;
        for (style, chunk) in ranges {
            let len = chunk.len();
            // Drop the trailing-newline token — it lives outside the
            // diff-row's content slice.
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

/// Pre-warm the static sets. Optional — first call to `highlight_line`
/// would warm them anyway. Call this from a background task at workspace
/// open if the ~30 ms cold-start latency on the first diff render shows
/// up in profiling. Default policy: don't pre-warm; let lazy init absorb
/// the cost.
pub fn prewarm() {
    let _ = (&*SYNTAX_SET, &*THEME_SET);
    // Touch the active theme too so the HashMap lookup is cached.
    let _ = active_theme();
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
