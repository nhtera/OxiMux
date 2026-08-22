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
//! - Each diff row is tokenized standalone via a fresh `ParseState` +
//!   `HighlightState` over the shared (cached) `Highlighter`, so a row can be
//!   fed one at a time without re-parsing the file or rebuilding the theme.
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

#[cfg(test)]
use syntect::highlighting::Style;
use syntect::highlighting::{HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

/// Lazy holder. `two_face::syntax::extra_no_newlines()` deserializes the
/// embedded `bat` grammar set (~250 languages) on first use. Read-only after
/// init; `Send + Sync` so the GPUI thread can read it. The `no_newlines`
/// variant matches the per-line feeding in `highlight_line`.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);

/// The diff syntax theme for the charcoal palette — a conventional
/// dark-editor token set (keyword-blue / string-orange / comment-green)
/// bundled as a TextMate theme and embedded at compile time. Only token
/// foregrounds are consumed; the theme's background is inert (OxiMux paints
/// its own surface).
static SYNTAX_DARK: LazyLock<Theme> = LazyLock::new(|| {
    let bytes = include_bytes!("../../../assets/themes/syntax-dark.tmTheme");
    ThemeSet::load_from_reader(&mut Cursor::new(&bytes[..]))
        .expect("bundled syntax-dark.tmTheme parses")
});

/// The Paper counterpart, scope for scope. A second file rather than a tint
/// applied to the first: token hues are not a hue rotation of each other, and
/// a diff highlighted in Dark+ colours on a white page is the most obvious
/// tell that a light theme was bolted on.
static SYNTAX_LIGHT: LazyLock<Theme> = LazyLock::new(|| {
    let bytes = include_bytes!("../../../assets/themes/syntax-light.tmTheme");
    ThemeSet::load_from_reader(&mut Cursor::new(&bytes[..]))
        .expect("bundled syntax-light.tmTheme parses")
});

/// The highlight theme for the palette in force.
fn active_theme(light: bool) -> &'static Theme {
    if light { &SYNTAX_LIGHT } else { &SYNTAX_DARK }
}

/// The syntect highlighter, derived once from the bundled theme. Building it
/// walks every scope selector in the theme to build its match caches, so it
/// must NOT be reconstructed per line. The original code created a fresh
/// `HighlightLines` (which rebuilds this) on every `highlight_line` call —
/// ~6 ms/line in a debug build, i.e. a ~1 s stall to highlight a 170-line
/// diff. Shared here so each line pays only a cheap `ParseState` +
/// `HighlightState` setup.
static HIGHLIGHTER_DARK: LazyLock<Highlighter<'static>> =
    LazyLock::new(|| Highlighter::new(active_theme(false)));

/// Its Paper counterpart. Two statics rather than one swapped at runtime:
/// building a `Highlighter` walks every scope selector in the theme, so it
/// must be paid once per palette and never per line — and a user who switches
/// back and forth should not re-pay it each time.
static HIGHLIGHTER_LIGHT: LazyLock<Highlighter<'static>> =
    LazyLock::new(|| Highlighter::new(active_theme(true)));

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
/// - no grammar matched the file (`Language::is_plain`), OR
/// - syntect's `highlight_line` fails (shouldn't happen on valid UTF-8 —
///   we log nothing because a syntax-highlight failure should never break
///   diff rendering).
///
/// The caller (`paint.rs`) treats empty output as "fall back to muted base
/// color", which keeps the renderer simple and prevents a bad grammar
/// match from blanking out the row.
pub fn highlight_line(line: &str, lang: Language, light: bool) -> Vec<HiToken> {
    let Some(syntax) = lang.0 else {
        return Vec::new();
    };
    if line.is_empty() {
        return Vec::new();
    }
    // Each diff row is highlighted standalone — a fresh parse + highlight
    // state per line — preserving the prior per-line behaviour (added/removed
    // rows must not leak multi-line string/comment state across the +/-
    // boundary). Only the heavy `Highlighter` (theme scope cache) is shared.
    let highlighter: &Highlighter<'static> = if light {
        &HIGHLIGHTER_LIGHT
    } else {
        &HIGHLIGHTER_DARK
    };
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

/// Pre-warm the static sets. Called from a background task at app boot
/// (`main.rs`) so the ~30 ms cold-start never lands on the first diff
/// paint. Safe to call any number of times; first call to
/// `highlight_line` would warm them anyway.
pub fn prewarm() {
    // Both palettes: the cost is a one-off parse each, and warming only the
    // one in force would move the stall to whenever the user switched.
    let _ = (&*SYNTAX_SET, &*SYNTAX_DARK, &*SYNTAX_LIGHT);
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
        assert!(highlight_line("", detect_language(&PathBuf::from("x.rs")), false).is_empty());
    }

    #[test]
    fn highlight_plain_language_yields_no_tokens() {
        let plain = detect_language(&PathBuf::from("notes.unknownext"));
        assert!(plain.is_plain());
        assert!(highlight_line("anything goes", plain, false).is_empty());
    }

    #[test]
    fn highlight_python_produces_tokens() {
        // Python was absent from the original 6-language list; the
        // extension-based resolver must now tokenize it.
        let lang = detect_language(&PathBuf::from("app.py"));
        assert!(!lang.is_plain(), "python should resolve");
        assert!(
            !highlight_line("def main():", lang, false).is_empty(),
            "python keyword line should produce tokens"
        );
    }

    #[test]
    fn highlight_rust_keyword_and_string_get_different_colors() {
        // `let` (keyword) and `"hello"` (string) should land on different
        // foreground colors under any sane theme. The exact values are
        // theme-dependent so we assert on inequality only.
        let toks = highlight_line(r#"let x = "hello";"#, detect_language(&PathBuf::from("x.rs")), false);
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
        let toks = highlight_line(r#"let x = "hello";"#, detect_language(&PathBuf::from("x.rs")), false);
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
        let toks = highlight_line(line, detect_language(&PathBuf::from("x.rs")), false);
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
    /// The two bundled themes must cover the same scopes.
    ///
    /// A scope present in one and missing from the other is a construct that
    /// highlights in charcoal and falls back to the base foreground on paper
    /// (or the reverse) — which reads as a rendering bug, not as a theme, and
    /// is invisible to anyone who only ever uses one palette.
    #[test]
    fn the_two_bundled_themes_cover_the_same_scopes() {
        fn scopes(src: &str) -> Vec<String> {
            src.split("<key>scope</key>")
                .skip(1)
                .filter_map(|chunk| {
                    let start = chunk.find("<string>")? + "<string>".len();
                    let end = chunk[start..].find("</string>")? + start;
                    Some(chunk[start..end].to_string())
                })
                .collect()
        }
        let dark = include_str!("../../../assets/themes/syntax-dark.tmTheme");
        let light = include_str!("../../../assets/themes/syntax-light.tmTheme");
        let (d, l) = (scopes(dark), scopes(light));
        assert!(!d.is_empty(), "the dark theme should declare scopes");
        assert_eq!(d, l, "scope lists diverged between the two syntax themes");
    }

    /// The palette actually reaches the tokens. Without this the light theme
    /// could be bundled, parsed, and never consulted — which is exactly the
    /// state the diff body was in when Paper first shipped.
    #[test]
    fn the_two_palettes_colour_the_same_line_differently() {
        let lang = detect_language(&PathBuf::from("x.rs"));
        let line = r#"let x = "hello";"#;
        let dark = highlight_line(line, lang, false);
        let light = highlight_line(line, lang, true);
        assert!(!dark.is_empty() && !light.is_empty(), "rust tokenizes");
        assert_eq!(
            dark.len(),
            light.len(),
            "same grammar, same spans -- only the colours may differ"
        );
        assert!(
            dark.iter().zip(&light).any(|(a, b)| (a.r, a.g, a.b) != (b.r, b.g, b.b)),
            "the two palettes produced identical colours"
        );
    }

    /// And each one lands on the right side of the ground it is drawn on: a
    /// keyword that is light on charcoal must be dark on paper.
    #[test]
    fn each_palette_tokenises_for_its_own_ground() {
        let lang = detect_language(&PathBuf::from("x.rs"));
        let line = "let x = 1;";
        let mean = |toks: Vec<HiToken>| {
            let n = toks.len().max(1) as u32;
            let sum: u32 = toks.iter().map(|t| t.r as u32 + t.g as u32 + t.b as u32).sum();
            sum / (3 * n)
        };
        let dark = mean(highlight_line(line, lang, false));
        let light = mean(highlight_line(line, lang, true));
        assert!(dark > 128, "charcoal tokens should be light (mean {dark})");
        assert!(light < 128, "paper tokens should be dark (mean {light})");
    }
}
