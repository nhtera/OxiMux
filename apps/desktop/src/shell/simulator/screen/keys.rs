//! Keyboard capture: a click on the screen focuses it, and while focused
//! keys go to the device as HID usages. ⌃Esc (or a click elsewhere) gives
//! the keyboard back.
//!
//! Two keys never reach `on_key_down`, because app-wide bindings match
//! first: bare Esc is `DismissOverlay` and ⌘V is the menu's `Paste`. The
//! screen handles both as actions, which the focused element sees before the
//! root. Other ⌘ and ⌃ chords are left to the app. Only ASCII is typed;
//! anything else (an IME's composed text) goes through paste.

use std::time::{Duration, Instant};

use gpui::{App, Context, KeyBinding, KeyDownEvent, NoAction, Window};
use oximux_simulator::keyboard::{self, KeyEvent};
use oximux_simulator::protocol::{Command, KeyPhase};

use super::{SIMULATOR_SCREEN_KEY_CONTEXT, ScreenView};
use crate::actions::DismissOverlay;
use crate::platform::menu::Paste;

/// How long the capture hint stays up.
const HINT: Duration = Duration::from_secs(3);
/// HID usages not in `keyboard::named_key`'s table.
const USAGE_ESCAPE: u32 = 0x29;
const USAGE_LEFT_SHIFT: u32 = 0xe1;

/// Shadow the focus-cycling Tab / Shift-Tab bindings while the screen has the
/// keyboard (the terminal's pattern): with no action to run, the keystroke
/// falls through to `on_key_down` and reaches the device. Call once at boot.
pub fn register_screen_key_bindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NoAction, Some(SIMULATOR_SCREEN_KEY_CONTEXT)),
        KeyBinding::new("shift-tab", NoAction, Some(SIMULATOR_SCREEN_KEY_CONTEXT)),
    ]);
}

/// GPUI's name for a non-printing key → `keyboard::named_key`'s name.
fn named(key: &str) -> Option<u32> {
    let name = match key {
        "enter" => "return",
        "tab" | "backspace" | "delete" | "up" | "down" | "left" | "right" | "home" | "end" => key,
        _ => return None,
    };
    keyboard::named_key(name)
}

/// The HID events for one key press, or `None` to leave it to the app.
pub(super) fn key_events(key: &str, key_char: Option<&str>, shift: bool) -> Option<Vec<KeyEvent>> {
    if let Some(usage) = named(key) {
        let mut events = vec![KeyEvent { usage, down: true }, KeyEvent { usage, down: false }];
        if shift {
            events.insert(0, KeyEvent { usage: USAGE_LEFT_SHIFT, down: true });
            events.push(KeyEvent { usage: USAGE_LEFT_SHIFT, down: false });
        }
        return Some(events);
    }
    let text = key_char.filter(|c| !c.is_empty() && c.is_ascii())?;
    keyboard::text_to_key_events(text).ok()
}

impl ScreenView {
    /// Take the keyboard (deferred: a focus set inside mouse-down is undone
    /// by GPUI's own post-click focus handling).
    pub(super) fn capture_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus.is_focused(window) {
            return;
        }
        let focus = self.focus.clone();
        window.defer(cx, move |window, cx| window.focus(&focus, cx));
        self.hint_until = Some(Instant::now() + HINT);
        self._hint = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HINT).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    pub(super) fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        if m.control && keystroke.key == "escape" {
            window.blur(cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if m.platform || m.control || m.function {
            return;
        }
        let Some(events) = key_events(&keystroke.key, keystroke.key_char.as_deref(), m.shift) else { return };
        cx.stop_propagation();
        self.send_keys(&events, cx);
    }

    fn send_keys(&self, events: &[KeyEvent], cx: &Context<Self>) {
        for k in events {
            let phase = if k.down { KeyPhase::Down } else { KeyPhase::Up };
            self.send(&Command::Key { phase, usage: k.usage }, cx);
        }
    }

    pub(super) fn on_escape(&mut self, _: &DismissOverlay, _window: &mut Window, cx: &mut Context<Self>) {
        let usage = USAGE_ESCAPE;
        self.send_keys(&[KeyEvent { usage, down: true }, KeyEvent { usage, down: false }], cx);
    }

    pub(super) fn on_paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(udid) = self.binding.device.clone() else { return };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        self.hub.update(cx, |hub, cx| hub.paste(&udid, text, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usages(events: &[KeyEvent]) -> Vec<(u32, bool)> {
        events.iter().map(|k| (k.usage, k.down)).collect()
    }

    #[test]
    fn named_keys_press_and_release_with_shift_around_them() {
        assert_eq!(usages(&key_events("enter", None, false).unwrap()), [(0x28, true), (0x28, false)]);
        assert_eq!(
            usages(&key_events("tab", None, true).unwrap()),
            [(0xe1, true), (0x2b, true), (0x2b, false), (0xe1, false)]
        );
        assert_eq!(usages(&key_events("left", None, false).unwrap())[0], (0x50, true));
    }

    #[test]
    fn typed_ascii_goes_through_the_us_layout_and_the_rest_is_left_alone() {
        // `key_char` already carries the shift ("A"), which the map re-applies.
        assert_eq!(usages(&key_events("a", Some("A"), true).unwrap()), [(0xe1, true), (0x04, true), (0x04, false), (0xe1, false)]);
        assert_eq!(usages(&key_events("space", Some(" "), false).unwrap()), [(0x2c, true), (0x2c, false)]);
        assert!(key_events("a", Some("å"), false).is_none(), "non-ASCII goes through paste");
        assert!(key_events("f5", None, false).is_none());
    }
}
