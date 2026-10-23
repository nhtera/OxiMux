//! Contrast math shared by every surface that paints colors it did not choose:
//! the diff view's syntax tokens and the terminal's program-chosen text.
//!
//! WCAG 2.x relative luminance and contrast ratio, plus two ways to lift a
//! foreground until it clears a minimum against its background:
//! - [`ensure_minimum_contrast`] steps HSL lightness (the diff view's original,
//!   kept byte-identical);
//! - [`ensure_minimum_contrast_oklab`] searches OKLab lightness, which keeps
//!   the perceived hue: a red error stays red on a light theme, only darker.

use gpui::{Hsla, Rgba};

/// Alpha-composite a translucent `top` over an opaque `base`, returning the
/// resulting opaque color — used to resolve a diff row's effective on-screen
/// background (translucent tint over the panel base) for contrast checks.
pub fn composite_over(top: Hsla, base: Hsla) -> Hsla {
    let t = Rgba::from(top);
    let b = Rgba::from(base);
    // Callers pass the opaque panel/base color; the blend below assumes it.
    debug_assert!((b.a - 1.0).abs() < 1e-4, "composite_over base must be opaque");
    let a = t.a;
    Hsla::from(Rgba {
        r: t.r * a + b.r * (1.0 - a),
        g: t.g * a + b.g * (1.0 - a),
        b: t.b * a + b.b * (1.0 - a),
        a: 1.0,
    })
}

/// Linearize one sRGB channel (gamma-expand) for relative-luminance, per the
/// WCAG 2.x definition (normative `0.04045` knee).
pub fn channel_lin(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance of a color.
pub fn relative_luminance(c: Hsla) -> f32 {
    let r = Rgba::from(c);
    0.2126 * channel_lin(r.r) + 0.7152 * channel_lin(r.g) + 0.0722 * channel_lin(r.b)
}

/// WCAG contrast ratio between two colors (≥ 1.0).
pub fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Nudge a syntax token's lightness until it clears `min` contrast against
/// the (composited) row background, preserving hue + saturation so the token
/// keeps its syntactic color. Pushes toward white on dark backgrounds, black
/// on light ones; stops at the first passing step or a lightness bound.
pub fn ensure_minimum_contrast(fg: Hsla, bg: Hsla, min: f32) -> Hsla {
    if contrast_ratio(fg, bg) >= min {
        return fg;
    }
    let step = if relative_luminance(bg) < 0.5 { 0.04 } else { -0.04 };
    let mut out = fg;
    // 26 steps of 0.04 span the full [0,1] lightness range, so the loop can
    // always reach the brightest/darkest end before giving up.
    for _ in 0..26 {
        let next = (out.l + step).clamp(0.0, 1.0);
        if (next - out.l).abs() < f32::EPSILON {
            break; // hit a clamp bound — can't push further
        }
        out.l = next;
        if contrast_ratio(out, bg) >= min {
            break;
        }
    }
    out
}

// ── OKLab ──────────────────────────────────────────────────────────────────
// Björn Ottosson's matrices, linear sRGB ↔ OKLab.

/// A color in OKLab: `l` perceived lightness (0 black … 1 white), `a`/`b` the
/// opponent axes whose angle is the hue and whose length is the chroma.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklab {
    pub l: f32,
    pub a: f32,
    pub b: f32,
}

fn channel_gamma(c: f32) -> f32 {
    if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

pub fn to_oklab(c: Hsla) -> Oklab {
    let s = Rgba::from(c);
    let (r, g, b) = (channel_lin(s.r), channel_lin(s.g), channel_lin(s.b));
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    Oklab {
        l: 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        a: 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        b: 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    }
}

/// Linear sRGB of an OKLab color, possibly outside `[0, 1]` (out of gamut).
fn oklab_to_linear(c: Oklab) -> [f32; 3] {
    let l = (c.l + 0.396_337_78 * c.a + 0.215_803_76 * c.b).powi(3);
    let m = (c.l - 0.105_561_346 * c.a - 0.063_854_17 * c.b).powi(3);
    let s = (c.l - 0.089_484_18 * c.a - 1.291_485_5 * c.b).powi(3);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
}

fn in_gamut(rgb: [f32; 3]) -> bool {
    rgb.iter().all(|c| (-1e-4..=1.0 + 1e-4).contains(c))
}

/// Back to an opaque-or-`alpha` `Hsla`, shrinking chroma (hue kept) only as
/// far as needed to land inside the sRGB gamut.
pub fn from_oklab(c: Oklab, alpha: f32) -> Hsla {
    let mut rgb = oklab_to_linear(c);
    if !in_gamut(rgb) {
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        for _ in 0..8 {
            let k = (lo + hi) / 2.0;
            if in_gamut(oklab_to_linear(Oklab { a: c.a * k, b: c.b * k, ..c })) {
                lo = k;
            } else {
                hi = k;
            }
        }
        rgb = oklab_to_linear(Oklab { a: c.a * lo, b: c.b * lo, ..c });
    }
    let g = |v: f32| channel_gamma(v.clamp(0.0, 1.0));
    Hsla::from(Rgba { r: g(rgb[0]), g: g(rgb[1]), b: g(rgb[2]), a: alpha })
}

/// Lift `fg` until it clears `min` contrast against opaque `bg`, changing only
/// OKLab lightness (chroma shrinks only where the gamut forces it), so the hue
/// a program chose survives. Moves away from the background on the side the
/// text already sits — light text gets lighter, dark text darker — by the
/// smallest amount that passes, crossing over only when that side cannot get
/// there. A pair that already passes — the common case — returns untouched
/// after one luminance check. `fg`'s alpha is kept, so callers can still fade
/// it after.
pub fn ensure_minimum_contrast_oklab(fg: Hsla, bg: Hsla, min: f32) -> Hsla {
    if min <= 1.0 || contrast_ratio(fg, bg) >= min {
        return fg;
    }
    let lab = to_oklab(fg);
    let at = |l: f32| from_oklab(Oklab { l, ..lab }, fg.a);
    let own_side = if relative_luminance(fg) > relative_luminance(bg) { 1.0 } else { 0.0 };
    let target = if contrast_ratio(at(own_side), bg) >= min {
        own_side
    } else if contrast_ratio(at(1.0 - own_side), bg) >= min {
        1.0 - own_side
    } else {
        // Unreachable even at the extremes: give the most there is. Below this
        // luminance white is the stronger extreme, above it black.
        return at(if relative_luminance(bg) < 0.18 { 1.0 } else { 0.0 });
    };
    // Binary search for the passing lightness closest to the original.
    let (mut near, mut far) = (lab.l, target);
    for _ in 0..8 {
        let mid = (near + far) / 2.0;
        if contrast_ratio(at(mid), bg) >= min {
            far = mid;
        } else {
            near = mid;
        }
    }
    at(far)
}

/// Entries kept by [`ensure_minimum_contrast_oklab_cached`] before it starts
/// over. Sized to hold a whole 200×60 screen of distinct colors (measured: a
/// 4096 cap thrashed on exactly that screen and saved nothing), ~0.5 MB.
const MEMO_CAP: usize = 16_384;

thread_local! {
    /// `(fg, bg, min)` → corrected fg. Paint runs on one thread, and a screen
    /// repaints the same few hundred pairs every frame; a truecolor gradient
    /// is the worst case, at one search per distinct cell. Keyed by the colors
    /// themselves, so a theme change needs no invalidation.
    static MEMO: std::cell::RefCell<std::collections::HashMap<(u32, u32, u16), Hsla>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn pack(c: Hsla) -> u32 {
    let r = Rgba::from(c);
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (q(r.r) << 24) | (q(r.g) << 16) | (q(r.b) << 8) | q(r.a)
}

/// [`ensure_minimum_contrast_oklab`] behind a bounded per-thread memo, for the
/// paint path. Cleared whole when full: simpler than LRU, and a full table
/// means the screen is churning through colors anyway.
pub fn ensure_minimum_contrast_oklab_cached(fg: Hsla, bg: Hsla, min: f32) -> Hsla {
    if min <= 1.0 {
        return fg;
    }
    // The cheap check first: most pairs pass, and need no memo slot.
    if contrast_ratio(fg, bg) >= min {
        return fg;
    }
    let key = (pack(fg), pack(bg), (min * 100.0).round() as u16);
    if let Some(hit) = MEMO.with(|m| m.borrow().get(&key).copied()) {
        return hit;
    }
    let out = ensure_minimum_contrast_oklab(fg, bg, min);
    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= MEMO_CAP {
            m.clear();
        }
        m.insert(key, out);
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(l: f32) -> Hsla {
        Hsla {
            h: 0.0,
            s: 0.0,
            l,
            a: 1.0,
        }
    }

    const MIN_TOKEN_CONTRAST: f32 = 3.0;

    #[test]
    fn contrast_ratio_white_over_black_is_maximal() {
        let r = contrast_ratio(gray(1.0), gray(0.0));
        assert!((r - 21.0).abs() < 0.5, "white/black ≈ 21:1, got {r}");
    }

    #[test]
    fn ensure_contrast_lifts_low_contrast_token_on_dark_bg() {
        let bg = gray(0.12); // dark tinted row
        let fg = gray(0.18); // nearly invisible over it
        assert!(contrast_ratio(fg, bg) < MIN_TOKEN_CONTRAST);
        let fixed = ensure_minimum_contrast(fg, bg, MIN_TOKEN_CONTRAST);
        assert!(fixed.l > fg.l, "pushed lighter on a dark background");
        // Small slack absorbs f32 rounding + the 0.04 lightness step; it is
        // NOT a tolerance for under-delivering contrast.
        assert!(
            contrast_ratio(fixed, bg) >= MIN_TOKEN_CONTRAST - 0.05,
            "reaches (≈) the minimum contrast"
        );
    }

    #[test]
    fn ensure_contrast_preserves_already_legible_token() {
        let bg = gray(0.12);
        let fg = gray(0.9);
        let same = ensure_minimum_contrast(fg, bg, MIN_TOKEN_CONTRAST);
        assert_eq!(same.l, fg.l, "legible token is returned unchanged");
    }

    #[test]
    fn composite_over_respects_alpha_bounds() {
        let base = gray(0.1);
        let clear = Hsla { a: 0.0, ..gray(0.9) };
        let solid = Hsla { a: 1.0, ..gray(0.9) };
        // a=0 → base luminance; a=1 → top luminance.
        assert!(
            (relative_luminance(composite_over(clear, base)) - relative_luminance(base)).abs()
                < 1e-4
        );
        assert!(
            (relative_luminance(composite_over(solid, base)) - relative_luminance(gray(0.9))).abs()
                < 1e-3
        );
    }

    fn rgb(r: u8, g: u8, b: u8) -> Hsla {
        Hsla::from(Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 })
    }

    fn hue_deg(c: Hsla) -> f32 {
        let lab = to_oklab(c);
        lab.b.atan2(lab.a).to_degrees()
    }

    fn hue_delta(x: f32, y: f32) -> f32 {
        let d = (x - y).abs() % 360.0;
        d.min(360.0 - d)
    }

    #[test]
    fn oklab_round_trips() {
        for c in [rgb(255, 0, 0), rgb(10, 200, 90), rgb(30, 30, 200), rgb(128, 128, 128)] {
            let back = Rgba::from(from_oklab(to_oklab(c), 1.0));
            let orig = Rgba::from(c);
            for (x, y) in [(back.r, orig.r), (back.g, orig.g), (back.b, orig.b)] {
                assert!((x - y).abs() < 2e-3, "{c:?}: {x} vs {y}");
            }
        }
    }

    #[test]
    fn oklab_correction_reaches_the_minimum_and_keeps_the_hue_on_paper() {
        let paper = rgb(0xF7, 0xF5, 0xF0);
        for fg in [rgb(255, 255, 0), rgb(0, 255, 255), rgb(255, 80, 80), rgb(80, 255, 80), rgb(120, 120, 255)] {
            for min in [3.0, 4.5, 7.0] {
                let fixed = ensure_minimum_contrast_oklab(fg, paper, min);
                assert!(contrast_ratio(fixed, paper) >= min - 0.02, "{fg:?} @ {min}");
                assert!(hue_delta(hue_deg(fixed), hue_deg(fg)) < 2.0, "{fg:?} hue drifted");
                assert!(to_oklab(fixed).l <= to_oklab(fg).l, "never lightened on a light bg");
            }
        }
    }

    #[test]
    fn oklab_correction_lightens_on_a_dark_background() {
        let charcoal = rgb(0x15, 0x17, 0x1A);
        let fg = rgb(40, 40, 140);
        let fixed = ensure_minimum_contrast_oklab(fg, charcoal, 4.5);
        assert!(contrast_ratio(fixed, charcoal) >= 4.48);
        assert!(to_oklab(fixed).l > to_oklab(fg).l);
    }

    #[test]
    fn light_text_on_a_mid_background_gets_lighter_not_darker() {
        let status_bar = rgb(0x5F, 0x87, 0xAF);
        let gray = rgb(0xBC, 0xBC, 0xBC);
        let fixed = ensure_minimum_contrast_oklab(gray, status_bar, 3.0);
        assert!(contrast_ratio(fixed, status_bar) >= 2.98);
        assert!(to_oklab(fixed).l > to_oklab(gray).l, "stayed on its own (light) side");
    }

    #[test]
    fn a_passing_pair_and_off_are_untouched() {
        let paper = rgb(0xF7, 0xF5, 0xF0);
        let ink = rgb(0x1A, 0x1D, 0x21);
        assert_eq!(ensure_minimum_contrast_oklab(ink, paper, 4.5), ink);
        let yellow = rgb(255, 255, 0);
        assert_eq!(ensure_minimum_contrast_oklab(yellow, paper, 1.0), yellow);
    }

    #[test]
    fn alpha_is_kept_for_the_fade_applied_after() {
        let paper = rgb(0xF7, 0xF5, 0xF0);
        let faded = Hsla { a: 0.7, ..rgb(255, 255, 0) };
        assert!((ensure_minimum_contrast_oklab(faded, paper, 3.0).a - 0.7).abs() < 1e-6);
    }

    #[test]
    fn the_cached_path_agrees_with_the_direct_one() {
        let paper = rgb(0xF7, 0xF5, 0xF0);
        for i in 0..300u32 {
            let fg = rgb((i * 7 % 256) as u8, (i * 13 % 256) as u8, (i * 29 % 256) as u8);
            assert_eq!(
                ensure_minimum_contrast_oklab_cached(fg, paper, 3.0),
                ensure_minimum_contrast_oklab(fg, paper, 3.0)
            );
        }
    }

    /// Cost of a worst-case screen (200×60, every cell its own truecolor on the
    /// light theme): uncached vs the memo the paint path uses. Run with
    /// `cargo test --release … contrast_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn contrast_cost_of_a_truecolor_screen() {
        let paper = rgb(0xF7, 0xF5, 0xF0);
        let cells: Vec<Hsla> = (0..12_000u32)
            .map(|i| rgb((i % 256) as u8, (i / 47 % 256) as u8, (255 - i % 256) as u8))
            .collect();
        let time = |f: &dyn Fn(Hsla) -> Hsla| {
            let t = std::time::Instant::now();
            let mut acc = 0.0;
            for &c in &cells {
                acc += f(c).l;
            }
            (t.elapsed(), acc)
        };
        let (plain, _) = time(&|c| c);
        let (direct, _) = time(&|c| ensure_minimum_contrast_oklab(c, paper, 3.0));
        let _ = time(&|c| ensure_minimum_contrast_oklab_cached(c, paper, 3.0));
        let (warm, _) = time(&|c| ensure_minimum_contrast_oklab_cached(c, paper, 3.0));
        eprintln!("frame of 12k distinct cells: none {plain:?}, direct {direct:?}, memo (warm) {warm:?}");
    }
}
