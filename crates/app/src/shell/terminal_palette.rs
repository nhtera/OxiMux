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
        CellColor::Named(named) => named_to_hsla(named),
        CellColor::Indexed(idx) => indexed_to_hsla(idx),
        CellColor::Rgb(r, g, b) => rgb_to_hsla(r, g, b),
    }
}

/// 16-color palette tuned for the charcoal canvas. Slightly warmer + less
/// saturated than VGA so the eye lands on diff/agent output before chrome.
fn named_to_hsla(named: NamedColor16) -> Hsla {
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

/// xterm 256-palette index. Returns a stable byte-exact mapping — no theme
/// tinting (truecolor users get truecolor, palette users get palette).
fn indexed_to_hsla(idx: u8) -> Hsla {
    match idx {
        0..=15 => named_to_hsla(named_from_index(idx)),
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
