//! Terminal color → GPUI `Hsla` resolver.
//!
//! Sits between `oximux_pty::CellColor` and the charcoal theme. The 16 named
//! colors are picked to read well on the `BG_BASE` charcoal canvas — neither
//! the loud VGA palette nor a desaturated theme like Solarized. Indexed
//! colors 16..=255 follow xterm: 16..=231 is a 6×6×6 RGB cube, 232..=255 is
//! grayscale. Truecolor (`Rgb`) flows through unchanged.
//!
//! `CellColor::Default` resolves against `theme.fg_base` / `theme.bg_base`
//! depending on whether the cell is asking about a foreground or background
//! slot. Keeps theme-aware fallbacks centralised here so the renderer never
//! needs to know about either palette.

use gpui::{Hsla, rgb};
use oximux_pty::{CellColor, NamedColor16};
use oximux_settings::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRole {
    Fg,
    Bg,
}

/// Resolve `color` against the active `theme` for the given role.
pub fn resolve(color: CellColor, role: ColorRole, theme: &Theme) -> Hsla {
    match color {
        CellColor::Default => match role {
            ColorRole::Fg => theme.fg_base,
            ColorRole::Bg => theme.bg_base,
        },
        CellColor::Named(named) => named_to_hsla(named, theme),
        CellColor::Indexed(idx) => indexed_to_hsla(idx, theme),
        CellColor::Rgb(r, g, b) => rgb_to_hsla(r, g, b),
    }
}

/// The 16 named colors, for whichever canvas the terminal is painted on.
///
/// Two sets rather than one, because this is the palette that cannot survive
/// a polarity change. The charcoal set is tuned to carry on near-black; the
/// same values on a white page are pastel, and the ones a program picks for
/// emphasis — bright yellow for a warning, bright white for a heading — turn
/// into the least readable text on screen.
fn named_to_hsla(named: NamedColor16, theme: &Theme) -> Hsla {
    if theme.is_light() {
        return named_on_paper(named);
    }
    match named {
        NamedColor16::Black => rgb(0x15171A).into(),
        NamedColor16::Red => rgb(0xD26464).into(),
        NamedColor16::Green => rgb(0x6FA86A).into(),
        NamedColor16::Yellow => rgb(0xD9A441).into(),
        NamedColor16::Blue => rgb(0x5B97C9).into(),
        NamedColor16::Magenta => rgb(0xB07DCE).into(),
        NamedColor16::Cyan => rgb(0x7AB8C4).into(),
        NamedColor16::White => rgb(0xC6C8CB).into(),
        NamedColor16::BrightBlack => rgb(0x6B7177).into(),
        NamedColor16::BrightRed => rgb(0xE07E7E).into(),
        NamedColor16::BrightGreen => rgb(0x85C07F).into(),
        NamedColor16::BrightYellow => rgb(0xEAB95B).into(),
        NamedColor16::BrightBlue => rgb(0x77AEDB).into(),
        NamedColor16::BrightMagenta => rgb(0xC79BE0).into(),
        NamedColor16::BrightCyan => rgb(0x94CDD8).into(),
        NamedColor16::BrightWhite => rgb(0xE6E8EB).into(),
    }
}

/// The same sixteen roles, darkened to carry on a white page.
///
/// The pair that needs explaining is white and bright-white. On charcoal they
/// are the two lightest colors in the set, and a program printing a heading in
/// bright white means "make this stand out". Kept light here, that heading
/// would be invisible. So the light set reads the *role* rather than the name:
/// white is the ordinary foreground weight and bright white the emphatic one,
/// which on paper means a mid grey and a near-black. Black and bright black
/// stay dark and mid-grey respectively, so a program that draws a dim rule in
/// black still draws a dim rule.
fn named_on_paper(named: NamedColor16) -> Hsla {
    match named {
        NamedColor16::Black => rgb(0x1A1D21).into(),
        NamedColor16::Red => rgb(0xC0392B).into(),
        NamedColor16::Green => rgb(0x2E7D32).into(),
        NamedColor16::Yellow => rgb(0xB37400).into(),
        NamedColor16::Blue => rgb(0x1F6FB2).into(),
        NamedColor16::Magenta => rgb(0x8E44AD).into(),
        NamedColor16::Cyan => rgb(0x00838F).into(),
        NamedColor16::White => rgb(0x5C6570).into(),
        NamedColor16::BrightBlack => rgb(0x8A929C).into(),
        NamedColor16::BrightRed => rgb(0xD9534F).into(),
        NamedColor16::BrightGreen => rgb(0x3E9142).into(),
        NamedColor16::BrightYellow => rgb(0xC98A00).into(),
        NamedColor16::BrightBlue => rgb(0x2F80C9).into(),
        NamedColor16::BrightMagenta => rgb(0xA55FC4).into(),
        NamedColor16::BrightCyan => rgb(0x0E9AA7).into(),
        NamedColor16::BrightWhite => rgb(0x2B3138).into(),
    }
}

/// xterm 256-palette index. Returns a stable byte-exact mapping — no theme
/// tinting (truecolor users get truecolor, palette users get palette) — except
/// for the first sixteen, which ARE the named colors under a different
/// spelling and so follow the same palette they do.
fn indexed_to_hsla(idx: u8, theme: &Theme) -> Hsla {
    match idx {
        0..=15 => named_to_hsla(named_from_index(idx), theme),
        16..=231 => {
            let i = idx - 16;
            let r = cube_axis(i / 36);
            let g = cube_axis((i / 6) % 6);
            let b = cube_axis(i % 6);
            rgb_to_hsla(r, g, b)
        }
        _ => {
            // 232..=255 — 24-step grayscale ramp (`0x08..0xEE` step 10).
            let step = idx - 232;
            let v = 0x08 + step * 10;
            rgb_to_hsla(v, v, v)
        }
    }
}

fn cube_axis(v: u8) -> u8 {
    // xterm cube: 0 → 0, 1 → 95, 2 → 135, 3 → 175, 4 → 215, 5 → 255.
    match v {
        0 => 0,
        1 => 95,
        2 => 135,
        3 => 175,
        4 => 215,
        _ => 255,
    }
}

fn named_from_index(idx: u8) -> NamedColor16 {
    match idx {
        0 => NamedColor16::Black,
        1 => NamedColor16::Red,
        2 => NamedColor16::Green,
        3 => NamedColor16::Yellow,
        4 => NamedColor16::Blue,
        5 => NamedColor16::Magenta,
        6 => NamedColor16::Cyan,
        7 => NamedColor16::White,
        8 => NamedColor16::BrightBlack,
        9 => NamedColor16::BrightRed,
        10 => NamedColor16::BrightGreen,
        11 => NamedColor16::BrightYellow,
        12 => NamedColor16::BrightBlue,
        13 => NamedColor16::BrightMagenta,
        14 => NamedColor16::BrightCyan,
        _ => NamedColor16::BrightWhite,
    }
}

fn rgb_to_hsla(r: u8, g: u8, b: u8) -> Hsla {
    let packed = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
    rgb(packed).into()
}
