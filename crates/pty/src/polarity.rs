//! Which way round the host window's background reads — and how that gets
//! told to the programs running inside the terminal.
//!
//! A child program asks this question twice, by two unrelated mechanisms,
//! and picks its own colors from the answer:
//!
//! - `COLORFGBG`, read from the environment once at startup (vim's
//!   `background` detection, base16-shell, and a long tail of scripts).
//! - OSC 11 (`ESC ] 11 ; ? BEL`), queried over the TTY at runtime by fzf,
//!   delta, bat, and modern agent CLIs including Claude Code.
//!
//! Answer "dark" to either one while the window is painted white and the
//! program cooperatively picks a palette tuned for near-black, then emits it
//! as 24-bit truecolor. Those sequences carry explicit r/g/b, so they flow
//! straight through the named- and indexed-color mapping the renderer
//! applies — nothing downstream can correct them. The result is pastel text
//! on white that no theme work in the app can reach. The only fix is to stop
//! giving the wrong answer.
//!
//! This is process-wide rather than per-session on purpose: it describes the
//! window every terminal is painted into, not any one terminal. A per-session
//! value would have to be set identically on every session and would drift
//! the first time a spawn path forgot it.
//!
//! Note what this cannot do. A program reads the answer when it starts and
//! keeps the palette it chose. Switching theme repaints OxiMux's own chrome
//! immediately, but a shell that is already running has no reason to ask
//! again — its existing output keeps the colors it picked. New programs, and
//! anything that re-queries, follow at once.
//!
//! # Windows delivers only one of the two answers
//!
//! Measured on Windows 10 through ConPTY, in this order:
//!
//! - The child's OSC 11 query *does* reach our emulator (alacritty raises
//!   `ColorRequest` for slot 257), and the reply is written back to the
//!   master with no error.
//! - The child never receives it.
//! - A DSR cursor-position report — same emulator, same `PtyReply` event,
//!   same `write` call — *does* arrive, as `ESC [ 5;1 R`.
//!
//! So the write-back path is sound and this is not something to go looking
//! for in our code: ConPTY translates master-side bytes into console input
//! records, and it passes CSI replies through while dropping OSC ones. That
//! leaves `COLORFGBG` as the only channel that actually carries the polarity
//! on Windows, which is the main reason it is set here rather than left to
//! the OSC path alone. On a real tty the reply is just input bytes and both
//! channels work.
//!
//! Keep answering OSC 11 regardless. It is correct, it is what every non-
//! Windows target uses, and a future ConPTY may well deliver it.

use std::sync::atomic::{AtomicU8, Ordering};

/// Whether the terminal is being painted onto a dark or a light background.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BackgroundPolarity {
    /// Light text on a dark ground — the original charcoal cockpit.
    #[default]
    Dark,
    /// Dark text on a light ground.
    Light,
}

impl BackgroundPolarity {
    /// The value for [`COLOR_FG_BG`]: foreground and background as ANSI
    /// palette indices, separated by `;`.
    ///
    /// This is the shape xterm and rxvt set and the one consumers parse —
    /// they read the digits after the last `;` and test them against a
    /// "dark" set. 15-on-0 is white text on black; 0-on-15 the reverse.
    pub fn color_fg_bg(self) -> &'static str {
        match self {
            Self::Dark => "15;0",
            Self::Light => "0;15",
        }
    }

    fn from_wire(v: u8) -> Self {
        match v {
            1 => Self::Light,
            _ => Self::Dark,
        }
    }

    fn to_wire(self) -> u8 {
        match self {
            Self::Dark => 0,
            Self::Light => 1,
        }
    }
}

/// Environment variable naming the terminal's foreground/background pair.
pub const COLOR_FG_BG: &str = "COLORFGBG";

/// Dark, so every existing caller and test keeps the behavior it had before
/// there was a choice: the app opts into light explicitly.
static POLARITY: AtomicU8 = AtomicU8::new(0);

/// Tell the emulator which way the host window reads. Called by the app when
/// the appearance loads and whenever the user changes it.
pub fn set_background_polarity(polarity: BackgroundPolarity) {
    POLARITY.store(polarity.to_wire(), Ordering::Relaxed);
}

/// The polarity in force. Read when answering a color query and when
/// building a child's environment.
pub fn background_polarity() -> BackgroundPolarity {
    BackgroundPolarity::from_wire(POLARITY.load(Ordering::Relaxed))
}

/// Add `COLORFGBG` to a child's environment unless the caller already set it.
///
/// The caller wins because a spawn that names the variable is being explicit
/// about the terminal it is emulating (a test fixture, a recorded session);
/// silently overwriting that would make the override untestable.
pub fn apply_color_fg_bg(env: &mut Vec<(String, String)>, polarity: BackgroundPolarity) {
    if env.iter().any(|(k, _)| k == COLOR_FG_BG) {
        return;
    }
    env.push((COLOR_FG_BG.to_string(), polarity.color_fg_bg().to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    // The two values are not interchangeable and a swapped pair is invisible
    // until a user reports "vim picks the wrong theme", so pin the direction.
    #[test]
    fn color_fg_bg_puts_the_background_last() {
        assert_eq!(BackgroundPolarity::Dark.color_fg_bg(), "15;0");
        assert_eq!(BackgroundPolarity::Light.color_fg_bg(), "0;15");
    }

    // Consumers split on ';' and read the LAST field as the background, so
    // that field is what decides light vs dark. Assert on the parsed value
    // rather than the whole string: this is the bit that carries meaning.
    #[test]
    fn the_trailing_field_is_the_background_index() {
        let bg = |p: BackgroundPolarity| {
            p.color_fg_bg()
                .rsplit(';')
                .next()
                .unwrap()
                .parse::<u8>()
                .unwrap()
        };
        assert_eq!(bg(BackgroundPolarity::Dark), 0, "black background");
        assert_eq!(bg(BackgroundPolarity::Light), 15, "white background");
    }

    #[test]
    fn an_explicit_caller_value_is_not_overwritten() {
        let mut env = vec![(COLOR_FG_BG.to_string(), "7;4".to_string())];
        apply_color_fg_bg(&mut env, BackgroundPolarity::Light);
        assert_eq!(env.len(), 1, "should not have appended a second entry");
        assert_eq!(env[0].1, "7;4", "the caller's value should survive");
    }

    #[test]
    fn an_absent_value_is_added() {
        let mut env = vec![("TERM".to_string(), "xterm-256color".to_string())];
        apply_color_fg_bg(&mut env, BackgroundPolarity::Light);
        assert_eq!(
            env.iter().find(|(k, _)| k == COLOR_FG_BG).map(|(_, v)| v.as_str()),
            Some("0;15")
        );
    }

    // The default has to stay Dark: every spawn path and every test that
    // predates this module relies on it, and a flipped default would change
    // their behavior silently.
    #[test]
    fn the_default_is_dark() {
        assert_eq!(BackgroundPolarity::default(), BackgroundPolarity::Dark);
        assert_eq!(BackgroundPolarity::from_wire(0), BackgroundPolarity::Dark);
    }

    #[test]
    fn wire_round_trips() {
        for p in [BackgroundPolarity::Dark, BackgroundPolarity::Light] {
            assert_eq!(BackgroundPolarity::from_wire(p.to_wire()), p);
        }
    }
}
