//! Chord → display glyphs. Modifier order is ⌘ ⌃ ⌥ ⇧ — the convention the
//! app shipped with (⌘⇧E, ⌃⇧1) — NOT gpui's `Keystroke` Display, which
//! renders control as `^` and sorts ⇧ after ⌘.

/// Format a gpui chord string ("cmd-shift-t", multi-stroke "cmd-k cmd-b")
/// as display glyphs ("⌘⇧T", "⌘K ⌘B"). Unparsable tokens pass through
/// verbatim — display must never panic on user data.
pub fn format_chord(chord: &str) -> String {
    chord
        .split_whitespace()
        .map(format_stroke)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One glyph token per modifier + one for the key (["⌘", "⇧", "T"]) — for
/// UI that renders each key as its own chip. Multi-stroke chords flatten.
pub fn format_chord_tokens(chord: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for stroke in chord.split_whitespace() {
        let (mods, key) = split_stroke(stroke);
        tokens.extend(mods.into_iter().map(str::to_string));
        tokens.push(key_glyph(key));
    }
    tokens
}

fn format_stroke(stroke: &str) -> String {
    let (mods, key) = split_stroke(stroke);
    let mut out: String = mods.concat();
    out.push_str(&key_glyph(key));
    out
}

/// Split "secondary-shift-t" into ordered modifier glyphs + the bare key. The
/// key is everything after the last modifier token, so "cmd--" (cmd + minus)
/// and bare "-" survive.
///
/// `secondary` is gpui's platform-relative modifier and is what the default
/// chords are written in: it means Command on macOS and Control elsewhere, so
/// it has to land in a different bucket per platform. Missing it here would not
/// mis-render a glyph — it would fall through to the key branch and print the
/// whole chord as its own key name.
fn split_stroke(stroke: &str) -> (Vec<&'static str>, &str) {
    let (mut cmd, mut ctrl, mut alt, mut shift, mut func) = (false, false, false, false, false);
    let mut rest = stroke;
    while let Some((head, tail)) = rest.split_once('-') {
        let glyph = match head {
            "cmd" | "super" | "win" => &mut cmd,
            "secondary" if cfg!(target_os = "macos") => &mut cmd,
            "secondary" => &mut ctrl,
            "ctrl" => &mut ctrl,
            "alt" => &mut alt,
            "shift" => &mut shift,
            "fn" => &mut func,
            _ => break,
        };
        if tail.is_empty() {
            // "cmd-" would leave an empty key; treat the dash as the key.
            break;
        }
        *glyph = true;
        rest = tail;
    }

    let mut mods = Vec::new();
    if cmd {
        mods.push("⌘");
    }
    if ctrl {
        mods.push("⌃");
    }
    if alt {
        mods.push("⌥");
    }
    if shift {
        mods.push("⇧");
    }
    if func {
        mods.push("fn");
    }
    (mods, rest)
}

fn key_glyph(key: &str) -> String {
    match key {
        "enter" => "↩".to_string(),
        "escape" => "Esc".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "Space".to_string(),
        "backspace" => "⌫".to_string(),
        "delete" => "⌦".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" => "PgUp".to_string(),
        "pagedown" => "PgDn".to_string(),
        k if k.len() == 1 => k.to_ascii_uppercase(),
        k => {
            // F-keys and anything else named: capitalize the first letter.
            let mut chars = k.chars();
            match chars.next() {
                Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secondary_renders_as_the_platform_modifier() {
        // The chord the inventory actually ships. Whatever glyph this is, it
        // must not be the literal token — the failure mode of not handling
        // `secondary` is a settings pane full of "Secondary-shift-t".
        let rendered = format_chord("secondary-shift-t");
        assert!(
            !rendered.to_lowercase().contains("secondary"),
            "got {rendered}"
        );
        if cfg!(target_os = "macos") {
            assert_eq!(rendered, "⌘⇧T");
        } else {
            assert_eq!(rendered, "⌃⇧T");
        }
    }

    #[test]
    fn secondary_and_cmd_agree_on_macos() {
        // The migration's whole safety claim: rewriting `cmd-` to `secondary-`
        // changes nothing a macOS user sees.
        if cfg!(target_os = "macos") {
            assert_eq!(format_chord("secondary-k"), format_chord("cmd-k"));
        }
    }

    #[test]
    fn multi_stroke_chords_format_each_stroke() {
        let rendered = format_chord("secondary-k secondary-b");
        let expected = if cfg!(target_os = "macos") {
            "⌘K ⌘B"
        } else {
            "⌃K ⌃B"
        };
        assert_eq!(rendered, expected);
    }

    #[test]
    fn tokens_split_modifiers_from_the_key() {
        let tokens = format_chord_tokens("secondary-shift-e");
        let modifier = if cfg!(target_os = "macos") { "⌘" } else { "⌃" };
        assert_eq!(tokens, vec![modifier, "⇧", "E"]);
    }

    #[test]
    fn a_trailing_dash_is_the_key_not_an_empty_one() {
        // "secondary--" is the zoom-out chord: platform modifier plus minus.
        let rendered = format_chord("secondary--");
        let modifier = if cfg!(target_os = "macos") { "⌘" } else { "⌃" };
        assert_eq!(rendered, format!("{modifier}-"));
    }
}
