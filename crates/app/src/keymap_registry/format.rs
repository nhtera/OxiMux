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

/// Split "cmd-shift-t" into ordered modifier glyphs + the bare key. The key
/// is everything after the last modifier token, so "cmd--" (cmd + minus)
/// and bare "-" survive.
fn split_stroke(stroke: &str) -> (Vec<&'static str>, &str) {
    let (mut cmd, mut ctrl, mut alt, mut shift, mut func) = (false, false, false, false, false);
    let mut rest = stroke;
    while let Some((head, tail)) = rest.split_once('-') {
        let glyph = match head {
            "cmd" | "super" | "win" => &mut cmd,
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
