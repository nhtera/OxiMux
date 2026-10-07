//! The panel's input, as the iPhone runner's gestures.
//!
//! The panel speaks a live touch stream (begin, moves, end — the simulator
//! helper replays it finger by finger) and HID key presses. The runner takes
//! whole gestures, each a second or so: so a touch is watched to its end and
//! then sent as one `tap`, `longPress` or `drag`, and key presses become
//! text. Coordinates stay portrait-normalized here (`0..1` of the screen);
//! [`super::control`] turns them into the runner's points.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::keyboard;
use crate::protocol::{Command, KeyPhase, TouchPhase};

/// A finger held this long without moving is a long press.
pub const LONG_PRESS: Duration = Duration::from_millis(500);
/// A drag's time, as sent: the real one, within these bounds.
pub const DRAG_MIN: Duration = Duration::from_millis(100);
pub const DRAG_MAX: Duration = Duration::from_millis(2000);
/// A finger still this long before it lifts ends a drag without a fling.
pub const SETTLE: Duration = Duration::from_millis(150);
/// A move farther than this (of the screen's width or height) makes a drag.
const SLOP: f64 = 0.015;
/// Smaller moves than this do not count as the finger moving at all.
const STILL: f64 = 0.002;
/// The longest long press sent.
const LONG_PRESS_MAX: Duration = Duration::from_secs(10);

/// The most text one `type` takes (the runner's limit).
pub const MAX_TEXT: usize = 4000;

const USAGE_RETURN: u32 = 0x28;
const USAGE_BACKSPACE: u32 = 0x2a;
const USAGE_KEYPAD_ENTER: u32 = 0x58;
const SHIFTS: [u32; 2] = [0xe1, 0xe5];

/// Hint for input a real iPhone does not take.
pub const KEYS_HINT: &str = "Only text, Return and Delete reach a real iPhone";
pub const PINCH_HINT: &str = "Pinch is not available on a real iPhone";

/// One thing for the runner to do, in portrait-normalized coordinates.
#[derive(Clone, Debug, PartialEq)]
pub enum Gesture {
    Tap { at: (f64, f64), taps: u8 },
    LongPress { at: (f64, f64), ms: u64 },
    /// `hold`: ms the finger stayed put before moving (a long press that
    /// then drags: how iOS picks up an icon to move it).
    Drag { from: (f64, f64), to: (f64, f64), ms: u64, hold: u64, settle: u64 },
    Text(String),
    Return,
    Delete(u32),
    Button(RunnerButton),
}

/// The iPhone's hardware buttons the runner can press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerButton {
    Home,
    VolumeUp,
    VolumeDown,
    /// The panel only (no agent verb names it).
    Action,
}

impl RunnerButton {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::VolumeUp => "volumeUp",
            Self::VolumeDown => "volumeDown",
            Self::Action => "action",
        }
    }
}

impl Gesture {
    /// Folds `next` into `self` when both can be one runner command:
    /// characters into text, deletes into one count, a second tap on the
    /// same spot into a double tap, and drags into one (their motions
    /// added). `next` back when they cannot.
    pub fn merge(&mut self, next: Gesture) -> Option<Gesture> {
        match (self, next) {
            (Gesture::Text(text), Gesture::Text(more)) if text.chars().count() + more.chars().count() <= MAX_TEXT => {
                text.push_str(&more);
                None
            }
            (Gesture::Delete(count), Gesture::Delete(more)) => {
                *count = count.saturating_add(more).min(500);
                None
            }
            (Gesture::Tap { at, taps: taps @ 1 }, Gesture::Tap { at: again, taps: 1 }) if near(*at, again) => {
                *taps = 2;
                None
            }
            (Gesture::Drag { to, ms, settle, hold: 0, .. }, Gesture::Drag { from: f2, to: t2, ms: m2, settle: s2, hold: 0 }) => {
                let motion = (t2.0 - f2.0, t2.1 - f2.1);
                *to = (clamp01(to.0 + motion.0), clamp01(to.1 + motion.1));
                *ms = (*ms + m2).min(DRAG_MAX.as_millis() as u64);
                *settle = s2;
                None
            }
            (_, next) => Some(next),
        }
    }
}

struct Touch {
    from: (f64, f64),
    began: Instant,
    last: (f64, f64),
    /// When the finger last moved (more than [`STILL`]).
    moved_at: Instant,
    /// When it first went past [`SLOP`] (it is a drag from then on).
    dragged_at: Option<Instant>,
}

/// Turns the panel's touch and key stream into [`Gesture`]s.
#[derive(Default)]
pub struct InputMap {
    touch: Option<Touch>,
    shift: bool,
}

impl InputMap {
    /// What `command` adds up to, if anything yet; `Err` a hint for input a
    /// real iPhone does not take.
    pub fn feed(&mut self, command: &Command, now: Instant) -> Result<Option<Gesture>, &'static str> {
        match *command {
            Command::Touch { phase, x, y, .. } => Ok(self.touch(phase, (x, y), now)),
            Command::Multitouch { .. } => Err(PINCH_HINT),
            Command::Key { phase, usage } => self.key(phase, usage),
            // Home, however the panel names it (a Face ID phone's is a swipe).
            Command::Button { name: crate::Button::Home | crate::Button::SwipeHome } => Ok(Some(Gesture::Button(RunnerButton::Home))),
            _ => Err(KEYS_HINT),
        }
    }

    fn touch(&mut self, phase: TouchPhase, p: (f64, f64), now: Instant) -> Option<Gesture> {
        match phase {
            TouchPhase::Begin => {
                self.touch = Some(Touch { from: p, began: now, last: p, moved_at: now, dragged_at: None });
                None
            }
            TouchPhase::Move => {
                let touch = self.touch.as_mut()?;
                if distance(touch.last, p) > STILL {
                    touch.moved_at = now;
                }
                if touch.dragged_at.is_none() && distance(touch.from, p) > SLOP {
                    touch.dragged_at = Some(now);
                }
                touch.last = p;
                None
            }
            TouchPhase::End => {
                let touch = self.touch.take()?;
                let held = now.duration_since(touch.began);
                let dragged_at = touch.dragged_at.or((distance(touch.from, p) > SLOP).then_some(now));
                if let Some(dragged_at) = dragged_at {
                    let moved_at = if distance(touch.last, p) > STILL { now } else { touch.moved_at };
                    let still = now.duration_since(moved_at);
                    // Held before it moved: a long press first.
                    let before = dragged_at.duration_since(touch.began);
                    let hold = if before >= LONG_PRESS { before.min(LONG_PRESS_MAX) } else { Duration::ZERO };
                    return Some(Gesture::Drag {
                        from: touch.from,
                        to: p,
                        ms: (held - hold).clamp(DRAG_MIN, DRAG_MAX).as_millis() as u64,
                        hold: hold.as_millis() as u64,
                        settle: if still >= SETTLE { still.min(Duration::from_millis(500)).as_millis() as u64 } else { 0 },
                    });
                }
                if held >= LONG_PRESS {
                    return Some(Gesture::LongPress { at: touch.from, ms: held.min(LONG_PRESS_MAX).as_millis() as u64 });
                }
                Some(Gesture::Tap { at: touch.from, taps: 1 })
            }
        }
    }

    fn key(&mut self, phase: KeyPhase, usage: u32) -> Result<Option<Gesture>, &'static str> {
        if SHIFTS.contains(&usage) {
            self.shift = phase == KeyPhase::Down;
            return Ok(None);
        }
        if phase != KeyPhase::Down {
            return Ok(None);
        }
        match usage {
            USAGE_RETURN | USAGE_KEYPAD_ENTER => Ok(Some(Gesture::Return)),
            USAGE_BACKSPACE => Ok(Some(Gesture::Delete(1))),
            _ => typed(usage, self.shift).map(|c| Some(Gesture::Text(c.to_string()))).ok_or(KEYS_HINT),
        }
    }
}

/// The character a US-layout key types, with or without Shift — the
/// reverse of [`keyboard::text_to_key_events`].
fn typed(usage: u32, shift: bool) -> Option<char> {
    static TABLE: OnceLock<HashMap<(u32, bool), char>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = HashMap::new();
        for c in (0x20u8..0x7f).map(char::from).chain(['\t']) {
            let Ok(events) = keyboard::text_to_key_events(&c.to_string()) else { continue };
            let shifted = events.len() == 4;
            if let Some(key) = events.iter().find(|e| e.down && !SHIFTS.contains(&e.usage)) {
                table.entry((key.usage, shifted)).or_insert(c);
            }
        }
        table
    });
    table.get(&(usage, shift)).copied()
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

fn near(a: (f64, f64), b: (f64, f64)) -> bool {
    distance(a, b) <= SLOP * 2.0
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(phase: TouchPhase, x: f64, y: f64) -> Command {
        Command::Touch { phase, x, y, edge: 0 }
    }

    fn key(phase: KeyPhase, usage: u32) -> Command {
        Command::Key { phase, usage }
    }

    /// Feeds `steps` (command, ms after the start) and collects gestures.
    fn run(steps: &[(Command, u64)]) -> Vec<Gesture> {
        let start = Instant::now();
        let mut map = InputMap::default();
        steps.iter().filter_map(|(c, ms)| map.feed(c, start + Duration::from_millis(*ms)).unwrap()).collect()
    }

    #[test]
    fn a_click_is_a_tap_and_a_held_click_a_long_press() {
        assert_eq!(run(&[(touch(TouchPhase::Begin, 0.5, 0.5), 0), (touch(TouchPhase::End, 0.505, 0.5), 90)]), [Gesture::Tap { at: (0.5, 0.5), taps: 1 }]);
        assert_eq!(
            run(&[(touch(TouchPhase::Begin, 0.2, 0.3), 0), (touch(TouchPhase::Move, 0.201, 0.3), 300), (touch(TouchPhase::End, 0.201, 0.3), 700)]),
            [Gesture::LongPress { at: (0.2, 0.3), ms: 700 }]
        );
    }

    #[test]
    fn a_drag_keeps_its_time_within_bounds_and_settles_when_held_still() {
        // A quick flick: the minimum time, no settle (it flings).
        assert_eq!(
            run(&[(touch(TouchPhase::Begin, 0.5, 0.8), 0), (touch(TouchPhase::Move, 0.5, 0.5), 30), (touch(TouchPhase::End, 0.5, 0.2), 60)]),
            [Gesture::Drag { from: (0.5, 0.8), to: (0.5, 0.2), ms: 100, hold: 0, settle: 0 }]
        );
        // Moved, then held still 300 ms before lifting: it settles.
        assert_eq!(
            run(&[(touch(TouchPhase::Begin, 0.5, 0.8), 0), (touch(TouchPhase::Move, 0.5, 0.4), 400), (touch(TouchPhase::Move, 0.5, 0.4), 600), (touch(TouchPhase::End, 0.5, 0.4), 700)]),
            [Gesture::Drag { from: (0.5, 0.8), to: (0.5, 0.4), ms: 700, hold: 0, settle: 300 }]
        );
        // Held still 800 ms, then moved: picked up, then dragged.
        assert_eq!(
            run(&[(touch(TouchPhase::Begin, 0.3, 0.3), 0), (touch(TouchPhase::Move, 0.3, 0.3), 500), (touch(TouchPhase::Move, 0.6, 0.3), 800), (touch(TouchPhase::End, 0.6, 0.6), 1400)]),
            [Gesture::Drag { from: (0.3, 0.3), to: (0.6, 0.6), ms: 600, hold: 800, settle: 0 }]
        );
        // A slow drag is capped.
        let slow = run(&[(touch(TouchPhase::Begin, 0.1, 0.1), 0), (touch(TouchPhase::Move, 0.3, 0.3), 100), (touch(TouchPhase::End, 0.9, 0.9), 9000)]);
        assert!(matches!(slow[..], [Gesture::Drag { ms: 2000, .. }]));
    }

    #[test]
    fn an_end_without_a_begin_is_nothing() {
        assert_eq!(run(&[(touch(TouchPhase::Move, 0.5, 0.5), 0), (touch(TouchPhase::End, 0.5, 0.5), 10)]), []);
    }

    #[test]
    fn keys_become_text_return_and_delete() {
        let shift = 0xe1;
        let gestures = run(&[
            (key(KeyPhase::Down, shift), 0),
            (key(KeyPhase::Down, 0x04), 1),
            (key(KeyPhase::Up, 0x04), 2),
            (key(KeyPhase::Up, shift), 3),
            (key(KeyPhase::Down, 0x05), 4),
            (key(KeyPhase::Down, 0x2c), 5),
            (key(KeyPhase::Down, shift), 6),
            (key(KeyPhase::Down, 0x1e), 7),
            (key(KeyPhase::Up, shift), 8),
            (key(KeyPhase::Down, USAGE_BACKSPACE), 9),
            (key(KeyPhase::Down, USAGE_RETURN), 10),
        ]);
        let texts: String = gestures.iter().filter_map(|g| if let Gesture::Text(t) = g { Some(t.as_str()) } else { None }).collect();
        assert_eq!(texts, "Ab !");
        assert_eq!(gestures[gestures.len() - 2..], [Gesture::Delete(1), Gesture::Return]);
    }

    #[test]
    fn what_an_iphone_cannot_take_is_a_hint() {
        let mut map = InputMap::default();
        let now = Instant::now();
        assert_eq!(map.feed(&key(KeyPhase::Down, 0x4f), now), Err(KEYS_HINT), "an arrow key");
        assert_eq!(map.feed(&Command::Multitouch { phase: TouchPhase::Begin, x1: 0.4, y1: 0.4, x2: 0.6, y2: 0.6 }, now), Err(PINCH_HINT));
        assert_eq!(map.feed(&Command::Button { name: crate::Button::Lock }, now), Err(KEYS_HINT));
        assert_eq!(map.feed(&Command::Button { name: crate::Button::Home }, now), Ok(Some(Gesture::Button(RunnerButton::Home))));
        assert_eq!(map.feed(&Command::Button { name: crate::Button::SwipeHome }, now), Ok(Some(Gesture::Button(RunnerButton::Home))));
    }

    #[test]
    fn queued_gestures_merge_into_one_command() {
        let mut text = Gesture::Text("he".into());
        assert_eq!(text.merge(Gesture::Text("y".into())), None);
        assert_eq!(text, Gesture::Text("hey".into()));
        // Not past the runner's limit.
        let mut long = Gesture::Text("x".repeat(MAX_TEXT - 1));
        assert!(long.merge(Gesture::Text("yz".into())).is_some());
        let mut delete = Gesture::Delete(2);
        assert_eq!(delete.merge(Gesture::Delete(3)), None);
        assert_eq!(delete, Gesture::Delete(5));
        let mut tap = Gesture::Tap { at: (0.5, 0.5), taps: 1 };
        assert_eq!(tap.merge(Gesture::Tap { at: (0.51, 0.5), taps: 1 }), None);
        assert_eq!(tap, Gesture::Tap { at: (0.5, 0.5), taps: 2 });
        // A third tap, or one elsewhere, stays its own.
        assert!(tap.merge(Gesture::Tap { at: (0.5, 0.5), taps: 1 }).is_some());
        let mut tap = Gesture::Tap { at: (0.1, 0.1), taps: 1 };
        assert!(tap.merge(Gesture::Tap { at: (0.9, 0.9), taps: 1 }).is_some());
        // Two scroll drags add up (and stay on the screen).
        let mut drag = Gesture::Drag { from: (0.5, 0.6), to: (0.5, 0.4), ms: 200, hold: 0, settle: 0 };
        assert_eq!(drag.merge(Gesture::Drag { from: (0.5, 0.6), to: (0.5, 0.3), ms: 300, hold: 0, settle: 150 }), None);
        let Gesture::Drag { to, ms: 500, settle: 150, .. } = drag else { panic!("{drag:?}") };
        assert!((to.1 - 0.1).abs() < 1e-9, "{to:?}");
        // Past the edge: it stops at the edge.
        assert_eq!(drag.merge(Gesture::Drag { from: (0.5, 0.9), to: (0.5, 0.1), ms: 100, hold: 0, settle: 0 }), None);
        assert!(matches!(drag, Gesture::Drag { to: (_, 0.0), .. }));
        // A pick-up-and-move is its own gesture.
        let lift = Gesture::Drag { from: (0.5, 0.5), to: (0.6, 0.6), ms: 300, hold: 800, settle: 0 };
        assert!(drag.merge(lift).is_some());
        // Text after a tap is not merged.
        assert!(Gesture::Tap { at: (0.5, 0.5), taps: 1 }.merge(Gesture::Text("a".into())).is_some());
    }
}
