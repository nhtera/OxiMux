//! Mouse-event → PTY-bytes encoding for apps that enable mouse reporting
//! (vim, lazygit, htop, tmux). Pure functions over primitives so the SGR
//! (1006) and legacy X10/1000 byte formats are unit-testable without GPUI
//! event plumbing.
//!
//! Coordinate convention: callers pass 0-based `(row, col)`; the wire format
//! is 1-based, so every encoder adds 1. The X10 form additionally offsets by
//! 32 and cannot represent coordinates past column/row 223.

use oximux_pty::MouseMode;

/// What happened to the pointer. `Drag` is motion with a button held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseAction {
    Press,
    Release,
    Drag,
}

/// Logical button. Wheel is encoded separately via [`encode_scroll`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseBtn {
    Left,
    Middle,
    Right,
}

fn btn_code(b: MouseBtn) -> u32 {
    match b {
        MouseBtn::Left => 0,
        MouseBtn::Middle => 1,
        MouseBtn::Right => 2,
    }
}

/// xterm mouse modifier bits: shift=4, alt/meta=8, ctrl=16.
pub fn mod_bits(shift: bool, alt: bool, ctrl: bool) -> u32 {
    (shift as u32) * 4 + (alt as u32) * 8 + (ctrl as u32) * 16
}

/// Encode a button press/release/drag at 0-based `(row, col)`. Returns `None`
/// when the app isn't requesting the relevant report (so the caller falls
/// back to local selection).
pub fn encode_button(
    action: MouseAction,
    btn: MouseBtn,
    (row, col): (usize, usize),
    mods: u32,
    mode: &MouseMode,
) -> Option<Vec<u8>> {
    if !mode.any_reporting() {
        return None;
    }
    // Drag only when the app asked for motion/drag tracking.
    if action == MouseAction::Drag && !(mode.report_drag || mode.report_motion) {
        return None;
    }
    let motion = if action == MouseAction::Drag { 32 } else { 0 };
    if mode.sgr {
        let cb = btn_code(btn) + mods + motion;
        let terminator = if action == MouseAction::Release {
            'm'
        } else {
            'M'
        };
        Some(format!("\x1b[<{};{};{}{}", cb, col + 1, row + 1, terminator).into_bytes())
    } else {
        // X10/1000: release is button code 3; the button identity is lost.
        let base = if action == MouseAction::Release {
            3
        } else {
            btn_code(btn)
        };
        encode_x10(base + mods + motion, col, row)
    }
}

/// Encode a wheel tick. `up` = scroll toward older content (button 64 vs 65).
/// Returns `None` when the app isn't reporting mouse, so the caller can scroll
/// scrollback locally or translate to arrow keys on the alt-screen.
pub fn encode_scroll(
    up: bool,
    (row, col): (usize, usize),
    mods: u32,
    mode: &MouseMode,
) -> Option<Vec<u8>> {
    if !mode.any_reporting() {
        return None;
    }
    let cb = if up { 64 } else { 65 } + mods;
    if mode.sgr {
        Some(format!("\x1b[<{};{};{}M", cb, col + 1, row + 1).into_bytes())
    } else {
        encode_x10(cb, col, row)
    }
}

fn encode_x10(cb: u32, col: usize, row: usize) -> Option<Vec<u8>> {
    let cx = col + 1 + 32;
    let cy = row + 1 + 32;
    // Coordinates beyond 223 (255 after the +32 byte offset) are
    // unrepresentable in the legacy form — drop the report rather than wrap.
    if cx > 255 || cy > 255 {
        return None;
    }
    Some(vec![0x1b, b'[', b'M', (cb + 32) as u8, cx as u8, cy as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sgr() -> MouseMode {
        MouseMode {
            report_click: true,
            sgr: true,
            ..MouseMode::default()
        }
    }

    #[test]
    fn off_mode_reports_nothing() {
        let off = MouseMode::default();
        assert_eq!(
            encode_button(MouseAction::Press, MouseBtn::Left, (0, 0), 0, &off),
            None
        );
        assert_eq!(encode_scroll(true, (0, 0), 0, &off), None);
    }

    #[test]
    fn sgr_press_and_release_are_1_based() {
        // Left press at row 4, col 9 (0-based) → SGR `<0;10;5M`.
        assert_eq!(
            encode_button(MouseAction::Press, MouseBtn::Left, (4, 9), 0, &sgr()).unwrap(),
            b"\x1b[<0;10;5M"
        );
        // Release flips the terminator to lowercase `m`.
        assert_eq!(
            encode_button(MouseAction::Release, MouseBtn::Left, (4, 9), 0, &sgr()).unwrap(),
            b"\x1b[<0;10;5m"
        );
    }

    #[test]
    fn sgr_right_button_with_ctrl() {
        // Right (2) + ctrl (16) = 18.
        assert_eq!(
            encode_button(MouseAction::Press, MouseBtn::Right, (0, 0), mod_bits(false, false, true), &sgr())
                .unwrap(),
            b"\x1b[<18;1;1M"
        );
    }

    #[test]
    fn drag_requires_drag_mode_and_sets_motion_bit() {
        // report_click alone → drag suppressed.
        assert_eq!(
            encode_button(MouseAction::Drag, MouseBtn::Left, (0, 0), 0, &sgr()),
            None
        );
        // With drag mode, motion bit 32 is added → 0 + 32 = 32.
        let drag_mode = MouseMode {
            report_drag: true,
            sgr: true,
            ..MouseMode::default()
        };
        assert_eq!(
            encode_button(MouseAction::Drag, MouseBtn::Left, (2, 3), 0, &drag_mode).unwrap(),
            b"\x1b[<32;4;3M"
        );
    }

    #[test]
    fn sgr_scroll_up_down_codes() {
        assert_eq!(encode_scroll(true, (0, 0), 0, &sgr()).unwrap(), b"\x1b[<64;1;1M");
        assert_eq!(encode_scroll(false, (0, 0), 0, &sgr()).unwrap(), b"\x1b[<65;1;1M");
    }

    #[test]
    fn x10_form_offsets_by_32_and_drops_far_coords() {
        let x10 = MouseMode {
            report_click: true,
            sgr: false,
            ..MouseMode::default()
        };
        // Left press at (0,0) → ESC [ M, then 32, 33, 33.
        assert_eq!(
            encode_button(MouseAction::Press, MouseBtn::Left, (0, 0), 0, &x10).unwrap(),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // Column 300 is unrepresentable in X10 → None.
        assert_eq!(
            encode_button(MouseAction::Press, MouseBtn::Left, (0, 300), 0, &x10),
            None
        );
    }
}
