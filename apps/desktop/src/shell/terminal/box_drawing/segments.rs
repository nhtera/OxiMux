//! U+2500–U+257F lookup table mapping each code point to its four cardinal
//! segments (top/right/bottom/left → cell-center). Predicates derived from
//! the table answer "is this cell part of a continuous horizontal run?" and
//! "does this cell extend to the left/right edge?" — used by the canvas
//! merge pass to draw multi-cell spans as a single stroke.

/// Line weight for a single segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineWeight {
    Light,
    Heavy,
    Double,
}

/// Which segments a box-drawing character uses, each segment extending from
/// cell center to one of the four edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxSegments {
    pub top: Option<LineWeight>,
    pub bottom: Option<LineWeight>,
    pub left: Option<LineWeight>,
    pub right: Option<LineWeight>,
}

impl BoxSegments {
    const fn new(
        top: Option<LineWeight>,
        bottom: Option<LineWeight>,
        left: Option<LineWeight>,
        right: Option<LineWeight>,
    ) -> Self {
        Self {
            top,
            bottom,
            left,
            right,
        }
    }

    const fn horizontal(w: LineWeight) -> Self {
        Self::new(None, None, Some(w), Some(w))
    }
    const fn vertical(w: LineWeight) -> Self {
        Self::new(Some(w), Some(w), None, None)
    }
    const fn corner_tl(w: LineWeight) -> Self {
        Self::new(None, Some(w), None, Some(w))
    }
    const fn corner_tr(w: LineWeight) -> Self {
        Self::new(None, Some(w), Some(w), None)
    }
    const fn corner_bl(w: LineWeight) -> Self {
        Self::new(Some(w), None, None, Some(w))
    }
    const fn corner_br(w: LineWeight) -> Self {
        Self::new(Some(w), None, Some(w), None)
    }
    const fn t_left(w: LineWeight) -> Self {
        Self::new(Some(w), Some(w), None, Some(w))
    }
    const fn t_right(w: LineWeight) -> Self {
        Self::new(Some(w), Some(w), Some(w), None)
    }
    const fn t_top(w: LineWeight) -> Self {
        Self::new(None, Some(w), Some(w), Some(w))
    }
    const fn t_bottom(w: LineWeight) -> Self {
        Self::new(Some(w), None, Some(w), Some(w))
    }
    const fn cross(w: LineWeight) -> Self {
        Self::new(Some(w), Some(w), Some(w), Some(w))
    }
}

use LineWeight::*;

/// True for any code point in U+2500..=U+257F. Block elements (U+2580+) are
/// out of scope — they ship as crisp font glyphs already.
#[inline]
pub fn is_box_drawing_char(ch: char) -> bool {
    let code = ch as u32;
    (0x2500..=0x257F).contains(&code)
}

/// Returns the common horizontal weight if BOTH left and right segments
/// match — the merge pass keys on this to glue consecutive cells into one
/// continuous stroke.
pub fn get_horizontal_weight(ch: char) -> Option<LineWeight> {
    let s = get_box_segments(ch)?;
    match (s.left, s.right) {
        (Some(l), Some(r)) if l == r => Some(l),
        _ => None,
    }
}

/// True for the four rounded-corner code points U+256D..=U+2570. The paint
/// module handles these with quadratic curves instead of right angles.
#[inline]
pub fn is_rounded_corner(ch: char) -> bool {
    let code = ch as u32;
    (0x256D..=0x2570).contains(&code)
}

/// True when the character has a vector implementation — i.e. the segment
/// model can express it. Diagonals (U+2571..=U+2573) are in the box-drawing
/// range but can't be expressed as cardinal segments; this predicate keeps
/// them out of the substitution path so they fall through to the font face.
#[inline]
pub fn has_vector_impl(ch: char) -> bool {
    get_box_segments(ch).is_some()
}

/// Map a U+2500..=U+257F code point to its four segments. Returns `None`
/// for the three diagonal slashes (U+2571..=U+2573) which the segment
/// model can't express — those fall back to font glyphs.
pub fn get_box_segments(ch: char) -> Option<BoxSegments> {
    let code = ch as u32;
    if !(0x2500..=0x257F).contains(&code) {
        return None;
    }
    Some(match code {
        // ─ ━ │ ┃
        0x2500 => BoxSegments::horizontal(Light),
        0x2501 => BoxSegments::horizontal(Heavy),
        0x2502 => BoxSegments::vertical(Light),
        0x2503 => BoxSegments::vertical(Heavy),

        // Dashed (rendered solid; dash patterns add cost with no readability win).
        0x2504 | 0x2508 | 0x254C => BoxSegments::horizontal(Light),
        0x2505 | 0x2509 | 0x254D => BoxSegments::horizontal(Heavy),
        0x2506 | 0x250A | 0x254E => BoxSegments::vertical(Light),
        0x2507 | 0x250B | 0x254F => BoxSegments::vertical(Heavy),

        // Light corners.
        0x250C => BoxSegments::corner_tl(Light),
        0x2510 => BoxSegments::corner_tr(Light),
        0x2514 => BoxSegments::corner_bl(Light),
        0x2518 => BoxSegments::corner_br(Light),

        // Mixed-weight corners (heavy on one axis, light on the other).
        0x250D => BoxSegments::new(None, Some(Light), None, Some(Heavy)),
        0x2511 => BoxSegments::new(None, Some(Light), Some(Heavy), None),
        0x2515 => BoxSegments::new(Some(Light), None, None, Some(Heavy)),
        0x2519 => BoxSegments::new(Some(Light), None, Some(Heavy), None),
        0x250E => BoxSegments::new(None, Some(Heavy), None, Some(Light)),
        0x2512 => BoxSegments::new(None, Some(Heavy), Some(Light), None),
        0x2516 => BoxSegments::new(Some(Heavy), None, None, Some(Light)),
        0x251A => BoxSegments::new(Some(Heavy), None, Some(Light), None),

        // Heavy corners.
        0x250F => BoxSegments::corner_tl(Heavy),
        0x2513 => BoxSegments::corner_tr(Heavy),
        0x2517 => BoxSegments::corner_bl(Heavy),
        0x251B => BoxSegments::corner_br(Heavy),

        // T-junctions, light.
        0x251C => BoxSegments::t_left(Light),
        0x2524 => BoxSegments::t_right(Light),
        0x252C => BoxSegments::t_top(Light),
        0x2534 => BoxSegments::t_bottom(Light),

        // Mixed T-junctions (light vertical / heavy horizontal axis).
        0x251D => BoxSegments::new(Some(Light), Some(Light), None, Some(Heavy)),
        0x2525 => BoxSegments::new(Some(Light), Some(Light), Some(Heavy), None),
        0x252D => BoxSegments::new(None, Some(Light), Some(Heavy), Some(Light)),
        0x252E => BoxSegments::new(None, Some(Light), Some(Light), Some(Heavy)),
        0x2535 => BoxSegments::new(Some(Light), None, Some(Heavy), Some(Light)),
        0x2536 => BoxSegments::new(Some(Light), None, Some(Light), Some(Heavy)),

        // Mixed T-junctions (heavy vertical / light horizontal axis).
        0x251E => BoxSegments::new(Some(Heavy), Some(Light), None, Some(Light)),
        0x251F => BoxSegments::new(Some(Light), Some(Heavy), None, Some(Light)),
        0x2526 => BoxSegments::new(Some(Heavy), Some(Light), Some(Light), None),
        0x2527 => BoxSegments::new(Some(Light), Some(Heavy), Some(Light), None),
        0x252F => BoxSegments::new(None, Some(Heavy), Some(Light), Some(Light)),
        0x2537 => BoxSegments::new(Some(Heavy), None, Some(Light), Some(Light)),

        // Heavy T-junctions.
        0x2520 => BoxSegments::new(Some(Heavy), Some(Heavy), None, Some(Light)),
        0x2521 => BoxSegments::new(Some(Heavy), Some(Light), None, Some(Heavy)),
        0x2522 => BoxSegments::new(Some(Light), Some(Heavy), None, Some(Heavy)),
        0x2523 => BoxSegments::t_left(Heavy),
        0x2528 => BoxSegments::new(Some(Heavy), Some(Heavy), Some(Light), None),
        0x2529 => BoxSegments::new(Some(Heavy), Some(Light), Some(Heavy), None),
        0x252A => BoxSegments::new(Some(Light), Some(Heavy), Some(Heavy), None),
        0x252B => BoxSegments::t_right(Heavy),
        0x2530 => BoxSegments::new(None, Some(Heavy), Some(Heavy), Some(Light)),
        0x2531 => BoxSegments::new(None, Some(Heavy), Some(Light), Some(Heavy)),
        0x2532 => BoxSegments::new(None, Some(Light), Some(Heavy), Some(Heavy)),
        0x2533 => BoxSegments::t_top(Heavy),
        0x2538 => BoxSegments::new(Some(Heavy), None, Some(Heavy), Some(Light)),
        0x2539 => BoxSegments::new(Some(Heavy), None, Some(Light), Some(Heavy)),
        0x253A => BoxSegments::new(Some(Light), None, Some(Heavy), Some(Heavy)),
        0x253B => BoxSegments::t_bottom(Heavy),

        // Crosses (light, mixed, heavy).
        0x253C => BoxSegments::cross(Light),
        0x253D => BoxSegments::new(Some(Light), Some(Light), Some(Heavy), Some(Light)),
        0x253E => BoxSegments::new(Some(Light), Some(Light), Some(Light), Some(Heavy)),
        0x253F => BoxSegments::new(Some(Light), Some(Light), Some(Heavy), Some(Heavy)),
        0x2540 => BoxSegments::new(Some(Heavy), Some(Light), Some(Light), Some(Light)),
        0x2541 => BoxSegments::new(Some(Light), Some(Heavy), Some(Light), Some(Light)),
        0x2542 => BoxSegments::new(Some(Heavy), Some(Heavy), Some(Light), Some(Light)),
        0x2543 => BoxSegments::new(Some(Heavy), Some(Light), Some(Heavy), Some(Light)),
        0x2544 => BoxSegments::new(Some(Heavy), Some(Light), Some(Light), Some(Heavy)),
        0x2545 => BoxSegments::new(Some(Light), Some(Heavy), Some(Heavy), Some(Light)),
        0x2546 => BoxSegments::new(Some(Light), Some(Heavy), Some(Light), Some(Heavy)),
        0x2547 => BoxSegments::new(Some(Heavy), Some(Heavy), Some(Heavy), Some(Light)),
        0x2548 => BoxSegments::new(Some(Heavy), Some(Heavy), Some(Light), Some(Heavy)),
        0x2549 => BoxSegments::new(Some(Heavy), Some(Light), Some(Heavy), Some(Heavy)),
        0x254A => BoxSegments::new(Some(Light), Some(Heavy), Some(Heavy), Some(Heavy)),
        0x254B => BoxSegments::cross(Heavy),

        // Double-line straights, corners, T-junctions, cross.
        0x2550 => BoxSegments::horizontal(Double),
        0x2551 => BoxSegments::vertical(Double),
        0x2554 => BoxSegments::corner_tl(Double),
        0x2557 => BoxSegments::corner_tr(Double),
        0x255A => BoxSegments::corner_bl(Double),
        0x255D => BoxSegments::corner_br(Double),
        0x2552 => BoxSegments::new(None, Some(Light), None, Some(Double)),
        0x2553 => BoxSegments::new(None, Some(Double), None, Some(Light)),
        0x2555 => BoxSegments::new(None, Some(Light), Some(Double), None),
        0x2556 => BoxSegments::new(None, Some(Double), Some(Light), None),
        0x2558 => BoxSegments::new(Some(Light), None, None, Some(Double)),
        0x2559 => BoxSegments::new(Some(Double), None, None, Some(Light)),
        0x255B => BoxSegments::new(Some(Light), None, Some(Double), None),
        0x255C => BoxSegments::new(Some(Double), None, Some(Light), None),
        0x2560 => BoxSegments::t_left(Double),
        0x2563 => BoxSegments::t_right(Double),
        0x2566 => BoxSegments::t_top(Double),
        0x2569 => BoxSegments::t_bottom(Double),
        0x255E => BoxSegments::new(Some(Light), Some(Light), None, Some(Double)),
        0x255F => BoxSegments::new(Some(Double), Some(Double), None, Some(Light)),
        0x2561 => BoxSegments::new(Some(Light), Some(Light), Some(Double), None),
        0x2562 => BoxSegments::new(Some(Double), Some(Double), Some(Light), None),
        0x2564 => BoxSegments::new(None, Some(Light), Some(Double), Some(Double)),
        0x2565 => BoxSegments::new(None, Some(Double), Some(Light), Some(Light)),
        0x2567 => BoxSegments::new(Some(Light), None, Some(Double), Some(Double)),
        0x2568 => BoxSegments::new(Some(Double), None, Some(Light), Some(Light)),
        0x256C => BoxSegments::cross(Double),
        0x256A => BoxSegments::new(Some(Light), Some(Light), Some(Double), Some(Double)),
        0x256B => BoxSegments::new(Some(Double), Some(Double), Some(Light), Some(Light)),

        // Rounded corners — paint module emits quadratic curves at center.
        0x256D => BoxSegments::corner_tl(Light),
        0x256E => BoxSegments::corner_tr(Light),
        0x256F => BoxSegments::corner_br(Light),
        0x2570 => BoxSegments::corner_bl(Light),

        // Diagonals — segment model can't express them; fall back to font.
        0x2571..=0x2573 => return None,

        // Half-lines.
        0x2574 => BoxSegments::new(None, None, Some(Light), None),
        0x2575 => BoxSegments::new(Some(Light), None, None, None),
        0x2576 => BoxSegments::new(None, None, None, Some(Light)),
        0x2577 => BoxSegments::new(None, Some(Light), None, None),
        0x2578 => BoxSegments::new(None, None, Some(Heavy), None),
        0x2579 => BoxSegments::new(Some(Heavy), None, None, None),
        0x257A => BoxSegments::new(None, None, None, Some(Heavy)),
        0x257B => BoxSegments::new(None, Some(Heavy), None, None),

        // Mixed-weight half-lines.
        0x257C => BoxSegments::new(None, None, Some(Light), Some(Heavy)),
        0x257D => BoxSegments::new(Some(Light), Some(Heavy), None, None),
        0x257E => BoxSegments::new(None, None, Some(Heavy), Some(Light)),
        0x257F => BoxSegments::new(Some(Heavy), Some(Light), None, None),

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_box_drawing_char_covers_block() {
        assert!(is_box_drawing_char('─'));
        assert!(is_box_drawing_char('╿'));
        assert!(!is_box_drawing_char('A'));
        assert!(!is_box_drawing_char(' '));
        assert!(!is_box_drawing_char('█'));
    }

    #[test]
    fn get_box_segments_horizontal_and_vertical() {
        let h = get_box_segments('─').unwrap();
        assert_eq!(h.left, Some(Light));
        assert_eq!(h.right, Some(Light));
        assert!(h.top.is_none() && h.bottom.is_none());

        let v = get_box_segments('║').unwrap();
        assert_eq!(v.top, Some(Double));
        assert_eq!(v.bottom, Some(Double));
    }

    #[test]
    fn get_box_segments_corner_and_cross() {
        let tl = get_box_segments('┌').unwrap();
        assert_eq!(tl.right, Some(Light));
        assert_eq!(tl.bottom, Some(Light));
        assert!(tl.left.is_none() && tl.top.is_none());

        let cross = get_box_segments('╬').unwrap();
        assert_eq!(cross.top, Some(Double));
        assert_eq!(cross.right, Some(Double));
    }

    #[test]
    fn horizontal_weight_only_when_both_sides_match() {
        assert_eq!(get_horizontal_weight('─'), Some(Light));
        assert_eq!(get_horizontal_weight('━'), Some(Heavy));
        assert_eq!(get_horizontal_weight('═'), Some(Double));
        // ┌ has right but no left — not a continuous horizontal run.
        assert_eq!(get_horizontal_weight('┌'), None);
        // Mixed weights — also not mergeable.
        assert_eq!(get_horizontal_weight('╾'), None);
    }

    #[test]
    fn rounded_corners_classified() {
        assert!(is_rounded_corner('╭'));
        assert!(is_rounded_corner('╰'));
        assert!(!is_rounded_corner('┌'));
    }

    #[test]
    fn diagonals_return_none() {
        assert!(get_box_segments('╱').is_none());
        assert!(get_box_segments('╲').is_none());
        assert!(get_box_segments('╳').is_none());
    }
}
