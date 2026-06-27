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
//! - The grammar set comes from `two-face` (the `bat` Sublime collection,
//!   ~250 languages) — Rust, Python, Go, TypeScript, TOML, JavaScript,
//!   C/C++, Java, Ruby, PHP, shell, YAML, JSON, HTML, CSS, SQL, Swift,
//!   Kotlin, … — so no per-language grammar crate and no `build.rs` C compiles.
//! - `syntect-fancy` selects the pure-Rust `fancy-regex` engine instead of
//!   the C `onig` binding. No `.a` link on macOS.
//! - Per-line restart is built in via `HighlightLines` — easy to feed one
//!   diff row at a time without re-parsing the file.
//!
//! Detection is delegated to syntect's own extension lookup rather than a
//! hand-maintained language enum, so every grammar in the set lights up
//! automatically.
//!
//! NOTE: `two-face` bundles Sublime grammars under their own licenses;
//! `two_face::acknowledgement` exposes the attribution listing if an
//! about/licenses screen ever needs it.
//!
//! ### Threading + caching
//!
//! The `SyntaxSet` is loaded once via `LazyLock` on first call (~30 ms cold).
//! Subsequent calls reuse it. A second `LazyLock` holds the active theme.
//! Both are `Send + Sync` so they're safe to read from the GPUI thread.

use std::io::Cursor;
use std::path::Path;
use std::sync::LazyLock;

use syntect::easy::HighlightLines;
#[cfg(test)]
use syntect::highlighting::Style;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Lazy holder. `two_face::syntax::extra_no_newlines()` deserializes the
/// embedded `bat` grammar set (~250 languages) on first use. Read-only after
/// init; `Send + Sync` so the GPUI thread can read it. The `no_newlines`
/// variant matches the per-line feeding in `highlight_line`.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);

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

/// A resolved grammar handle for one file. Wraps the syntect
/// [`SyntaxReference`] matched from the bundled set, or `None` when nothing
/// fits (unknown extension, plain `.log`, …). Carrying the reference instead
/// of a fixed language enum means every grammar syntect ships highlights
/// with no per-language match arm. `Copy` so the renderer can thread it
/// through `tokens_for_row` per line for free.
#[derive(Debug, Copy, Clone)]
pub struct Language(Option<&'static SyntaxReference>);

impl Language {
    /// True when no grammar matched — the renderer reads this as "paint the
    /// row in the muted base color, no per-token syntax color".
    pub fn is_plain(&self) -> bool {
        self.0.is_none()
    }
}

/// Map a file path to a bundled grammar. Extension lookup first (covers the
/// vast majority of source files), then a short filename map for the
/// extension-less cases syntect ships a grammar for (`Makefile`). No shebang
/// sniffing or content heuristics — anything unmatched renders plain.
pub fn detect_language(path: &Path) -> Language {
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if let Some(syntax) = SYNTAX_SET.find_syntax_by_extension(&ext) {
            return Language(Some(syntax));
        }
    }
    if let Some(name) = path.file_name().and_then(|s| s.to_str())
        && let Some(syntax) = syntax_for_filename(name)
    {
        return Language(Some(syntax));
    }
    Language(None)
}

/// Resolve a grammar for the handful of extension-less filenames syntect
/// bundles a grammar for. Kept deliberately short — anything missing just
/// renders plain, which is the safe default.
fn syntax_for_filename(name: &str) -> Option<&'static SyntaxReference> {
    let grammar = match name {
        "Makefile" | "makefile" | "GNUmakefile" => "Makefile",
        _ => return None,
    };
    SYNTAX_SET.find_syntax_by_name(grammar)
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
    let Some(syntax) = lang.0 else {
        return Vec::new();
    };
    if line.is_empty() {
        return Vec::new();
    }
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
    fn detect_language_resolves_bundled_extensions() {
        // Every grammar syntect bundles by extension must resolve (no plain
        // fallback) — far beyond the original 6-language list. This is the
        // broad-coverage contract.
        for f in [
            "src/main.rs",
            "app.py",
            "server.go",
            "index.js",
            "App.tsx",
            "config.yaml",
            "Cargo.toml",
            "deploy.sh",
            "main.c",
            "engine.cpp",
            "Service.java",
            "model.rb",
            "index.php",
            "styles.css",
            "page.html",
            "query.sql",
            "init.lua",
            "View.swift",
            "Main.kt",
            "README.md",
            "package.json",
        ] {
            assert!(
                !detect_language(&PathBuf::from(f)).is_plain(),
                "{f} should resolve to a bundled grammar"
            );
        }
    }

    #[test]
    fn detect_language_case_insensitive() {
        assert!(!detect_language(&PathBuf::from("README.MD")).is_plain());
        assert!(!detect_language(&PathBuf::from("Foo.RS")).is_plain());
        assert!(!detect_language(&PathBuf::from("App.PY")).is_plain());
    }

    #[test]
    fn detect_language_filename_and_plain_fallback() {
        // Makefile's grammar is reachable only by filename (no extension).
        assert!(!detect_language(&PathBuf::from("Makefile")).is_plain());
        // No grammar for these — must render plain (the safe default).
        assert!(detect_language(&PathBuf::from("notes.unknownext")).is_plain());
        assert!(detect_language(&PathBuf::from("file.zzzznope")).is_plain());
    }

    #[test]
    fn highlight_empty_line_yields_no_tokens() {
        assert!(highlight_line("", detect_language(&PathBuf::from("x.rs"))).is_empty());
    }

    #[test]
    fn highlight_plain_language_yields_no_tokens() {
        let plain = detect_language(&PathBuf::from("notes.unknownext"));
        assert!(plain.is_plain());
        assert!(highlight_line("anything goes", plain).is_empty());
    }

    #[test]
    fn highlight_python_produces_tokens() {
        // Python was absent from the original 6-language list; the
        // extension-based resolver must now tokenize it.
        let lang = detect_language(&PathBuf::from("app.py"));
        assert!(!lang.is_plain(), "python should resolve");
        assert!(
            !highlight_line("def main():", lang).is_empty(),
            "python keyword line should produce tokens"
        );
    }

    #[test]
    fn highlight_rust_keyword_and_string_get_different_colors() {
        // `let` (keyword) and `"hello"` (string) should land on different
        // foreground colors under any sane theme. The exact values are
        // theme-dependent so we assert on inequality only.
        let toks = highlight_line(r#"let x = "hello";"#, detect_language(&PathBuf::from("x.rs")));
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
        let toks = highlight_line(r#"let x = "hello";"#, detect_language(&PathBuf::from("x.rs")));
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
        let toks = highlight_line(line, detect_language(&PathBuf::from("x.rs")));
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
