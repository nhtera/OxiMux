//! The panel's input commands (the iOS helper's [`Command`] vocabulary:
//! normalized touches, HID key usages, named buttons) as scrcpy control
//! messages, so the screen view, the toolbar and the agent verbs drive an
//! Android device without knowing it is one.
//!
//! Pointer coordinates arrive portrait-normalized, as for the iOS helper; the
//! Android stream is the display as shown (rotation included), so they are
//! mapped back to display space by the current orientation, then scaled to
//! the video's pixels. A message only lands when it names the video's current
//! size.

use super::keycode as k;
use super::scrcpy_control::{ControlMsg, FINGER, KeyAction, MotionAction, Position, SECOND_FINGER};
use crate::geometry::portrait_to_display;
use crate::protocol::{Command, KeyPhase, TouchPhase};
use crate::{Button, Orientation};

/// Buttons only Android has, beside the shared [`Button`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AndroidButton {
    Back,
    VolumeUp,
    VolumeDown,
}

/// Held modifiers, for the meta state each key event carries (injected
/// events do not update the device's own modifier state).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers(u32);

impl Modifiers {
    pub fn meta(self) -> u32 {
        self.0
    }
}

/// The video now: its size in pixels (as streamed, display orientation) and
/// the device orientation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    pub size: (u32, u32),
    pub orientation: Orientation,
}

/// Translate one command. `screen` is `None` before the first session packet,
/// which drops pointer input. Commands that are not input (configure,
/// screenshot, AX…) translate to nothing.
pub fn translate(command: &Command, screen: Option<Screen>, mods: &mut Modifiers) -> Vec<ControlMsg> {
    let at = |p: (f64, f64), s: Screen| position(portrait_to_display(s.orientation, p), s.size);
    match command {
        Command::Touch { phase, x, y, .. } => screen.map(|s| vec![touch(*phase, FINGER, at((*x, *y), s))]).unwrap_or_default(),
        Command::Multitouch { phase, x1, y1, x2, y2 } => screen
            .map(|s| vec![touch(*phase, FINGER, at((*x1, *y1), s)), touch(*phase, SECOND_FINGER, at((*x2, *y2), s))])
            .unwrap_or_default(),
        Command::Scroll { dx, dy, x, y } => screen
            .map(|s| {
                let at = at((x.unwrap_or(0.5), y.unwrap_or(0.5)), s);
                // Normalized deltas to wheel notches (a full screen ≈ 16).
                vec![ControlMsg::Scroll { position: at, hscroll: (*dx * 16.0) as f32, vscroll: (-*dy * 16.0) as f32, buttons: 0 }]
            })
            .unwrap_or_default(),
        Command::Key { phase, usage } => key(*phase, *usage, mods),
        Command::Button { name } => press(button_keycode(*name)),
        _ => Vec::new(),
    }
}

/// Down then up of an Android-only button.
pub fn android_button(button: AndroidButton) -> Vec<ControlMsg> {
    press(match button {
        AndroidButton::Back => k::BACK,
        AndroidButton::VolumeUp => k::VOLUME_UP,
        AndroidButton::VolumeDown => k::VOLUME_DOWN,
    })
}

fn touch(phase: TouchPhase, pointer_id: u64, position: Position) -> ControlMsg {
    let (action, pressure) = match phase {
        TouchPhase::Begin => (MotionAction::Down, 1.0),
        TouchPhase::Move => (MotionAction::Move, 1.0),
        TouchPhase::End => (MotionAction::Up, 0.0),
    };
    ControlMsg::Touch { action, pointer_id, position, pressure, action_button: 0, buttons: 0 }
}

fn position((x, y): (f64, f64), (w, h): (u32, u32)) -> Position {
    let px = |v: f64, max: u32| (v.clamp(0.0, 1.0) * f64::from(max)).round().min(f64::from(max.saturating_sub(1))) as i32;
    Position { x: px(x, w), y: px(y, h), width: w.min(u32::from(u16::MAX)) as u16, height: h.min(u32::from(u16::MAX)) as u16 }
}

fn press(keycode: u32) -> Vec<ControlMsg> {
    [KeyAction::Down, KeyAction::Up]
        .into_iter()
        .map(|action| ControlMsg::Keycode { action, keycode, repeat: 0, metastate: 0 })
        .collect()
}

fn button_keycode(button: Button) -> u32 {
    match button {
        Button::Home | Button::SwipeHome => k::HOME,
        Button::Lock | Button::SideButton => k::POWER,
        Button::AppSwitcher => k::APP_SWITCH,
        // The assistant, as Siri is on iOS.
        Button::Siri => 219,
    }
}

fn key(phase: KeyPhase, usage: u32, mods: &mut Modifiers) -> Vec<ControlMsg> {
    let action = match phase {
        KeyPhase::Down => KeyAction::Down,
        KeyPhase::Up => KeyAction::Up,
    };
    if let Some(flag) = modifier_flag(usage) {
        match phase {
            KeyPhase::Down => mods.0 |= flag,
            KeyPhase::Up => mods.0 &= !flag,
        }
    }
    let Some(keycode) = hid_to_keycode(usage) else { return Vec::new() };
    vec![ControlMsg::Keycode { action, keycode, repeat: 0, metastate: mods.meta() }]
}

/// The meta flag a modifier key sets (HID usages 0xE0–0xE7).
fn modifier_flag(usage: u32) -> Option<u32> {
    match usage {
        0xe0 | 0xe4 => Some(k::META_CTRL_ON),
        0xe1 | 0xe5 => Some(k::META_SHIFT_ON),
        0xe2 | 0xe6 => Some(k::META_ALT_ON),
        0xe3 | 0xe7 => Some(k::META_META_ON),
        _ => None,
    }
}

/// A USB HID keyboard usage (page 7) as an Android keycode.
pub fn hid_to_keycode(usage: u32) -> Option<u32> {
    Some(match usage {
        0x04..=0x1d => 29 + (usage - 0x04), // A..Z
        0x1e..=0x26 => 8 + (usage - 0x1e),  // 1..9
        0x27 => 7,                          // 0
        0x28 => k::ENTER,
        0x29 => k::ESCAPE,
        0x2a => k::DEL,
        0x2b => k::TAB,
        0x2c => 62, // space
        0x2d => 69, // minus
        0x2e => 70, // equals
        0x2f => 71, // [
        0x30 => 72, // ]
        0x31 => 73, // backslash
        0x33 => 74, // ;
        0x34 => 75, // '
        0x35 => 68, // `
        0x36 => 55, // ,
        0x37 => 56, // .
        0x38 => 76, // /
        0x39 => 115, // caps lock
        0x3a..=0x45 => 131 + (usage - 0x3a), // F1..F12
        0x4a => k::MOVE_HOME,
        0x4b => k::PAGE_UP,
        0x4c => k::FORWARD_DEL,
        0x4d => k::MOVE_END,
        0x4e => k::PAGE_DOWN,
        0x4f => k::DPAD_RIGHT,
        0x50 => k::DPAD_LEFT,
        0x51 => k::DPAD_DOWN,
        0x52 => k::DPAD_UP,
        0xe0 => 113, // ctrl left
        0xe1 => 59,  // shift left
        0xe2 => 57,  // alt left
        0xe3 => 117, // meta left
        0xe4 => 114,
        0xe5 => 60,
        0xe6 => 58,
        0xe7 => 118,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Option<Screen> = Some(Screen { size: (1080, 2400), orientation: Orientation::Portrait });

    #[test]
    fn a_tap_lands_in_video_pixels_with_a_finger() {
        let mut mods = Modifiers::default();
        let down = translate(&Command::Touch { phase: TouchPhase::Begin, x: 0.5, y: 0.25, edge: 0 }, SIZE, &mut mods);
        let up = translate(&Command::Touch { phase: TouchPhase::End, x: 1.0, y: 1.0, edge: 0 }, SIZE, &mut mods);
        let ControlMsg::Touch { action, pointer_id, position, pressure, .. } = &down[0] else { panic!("touch") };
        assert_eq!((*action, *pointer_id, *pressure), (MotionAction::Down, FINGER, 1.0));
        assert_eq!(*position, Position { x: 540, y: 600, width: 1080, height: 2400 });
        let ControlMsg::Touch { action, position, pressure, .. } = &up[0] else { panic!("touch") };
        assert_eq!((*action, *pressure), (MotionAction::Up, 0.0));
        assert_eq!((position.x, position.y), (1079, 2399), "the edge is the last pixel, not past it");
        assert!(translate(&Command::Touch { phase: TouchPhase::Begin, x: 0.5, y: 0.5, edge: 0 }, None, &mut mods).is_empty());
    }

    /// Rotated, the stream is landscape and touches arrive portrait-normalized:
    /// the portrait top-left corner is the display's bottom-left when turned
    /// counter-clockwise.
    #[test]
    fn a_rotated_touch_is_mapped_back_to_the_display() {
        let screen = Some(Screen { size: (2400, 1080), orientation: Orientation::LandscapeLeft });
        let msgs = translate(&Command::Touch { phase: TouchPhase::Begin, x: 0.0, y: 0.0, edge: 0 }, screen, &mut Modifiers::default());
        let ControlMsg::Touch { position, .. } = &msgs[0] else { panic!("touch") };
        assert_eq!((position.x, position.y, position.width, position.height), (0, 1079, 2400, 1080));
    }

    #[test]
    fn a_pinch_moves_two_fingers() {
        let msgs = translate(
            &Command::Multitouch { phase: TouchPhase::Move, x1: 0.4, y1: 0.5, x2: 0.6, y2: 0.5 },
            SIZE,
            &mut Modifiers::default(),
        );
        let ids: Vec<u64> = msgs.iter().map(|m| match m { ControlMsg::Touch { pointer_id, .. } => *pointer_id, _ => 0 }).collect();
        assert_eq!(ids, [FINGER, SECOND_FINGER]);
    }

    #[test]
    fn keys_map_and_carry_held_modifiers() {
        let mut mods = Modifiers::default();
        let key = |phase, usage, mods: &mut Modifiers| translate(&Command::Key { phase, usage }, SIZE, mods);
        key(KeyPhase::Down, 0xe1, &mut mods); // shift
        let a = key(KeyPhase::Down, 0x04, &mut mods);
        assert_eq!(a, [ControlMsg::Keycode { action: KeyAction::Down, keycode: 29, repeat: 0, metastate: k::META_SHIFT_ON }]);
        key(KeyPhase::Up, 0xe1, &mut mods);
        let enter = key(KeyPhase::Down, 0x28, &mut mods);
        assert_eq!(enter, [ControlMsg::Keycode { action: KeyAction::Down, keycode: k::ENTER, repeat: 0, metastate: 0 }]);
        assert_eq!(hid_to_keycode(0x27), Some(7));
        assert_eq!(hid_to_keycode(0x1e), Some(8));
        assert_eq!(hid_to_keycode(0x52), Some(k::DPAD_UP));
        assert_eq!(hid_to_keycode(0x99), None);
    }

    #[test]
    fn buttons_press_and_release() {
        let home = translate(&Command::Button { name: Button::Home }, SIZE, &mut Modifiers::default());
        assert_eq!(home.len(), 2);
        assert!(matches!(home[0], ControlMsg::Keycode { action: KeyAction::Down, keycode: k::HOME, .. }));
        assert!(matches!(home[1], ControlMsg::Keycode { action: KeyAction::Up, keycode: k::HOME, .. }));
        let back = android_button(AndroidButton::Back);
        assert!(matches!(back[0], ControlMsg::Keycode { keycode: k::BACK, .. }));
        assert!(translate(&Command::Screenshot, SIZE, &mut Modifiers::default()).is_empty());
    }
}
