//! Named-action keyboard-shortcut registry — the single source of truth
//! for every user-facing binding. The inventory table carries (id, label,
//! category, default chord, bind fn); the effective map is defaults ⊕ the
//! user's `keybindings.toml` overrides. Tooltips, the palette, and the
//! welcome screen format chords from here instead of hardcoding glyphs.
//!
//! Live rebinding appends to the GPUI keymap (later-added bindings win at
//! equal context depth) and shadows a replaced chord with [`NoAction`], so
//! the boot keymap never has to be cleared — clearing would also drop the
//! text-input bindings the component library installs at init.

mod format;
mod inventory;
mod rebind;

pub use format::{format_chord, format_chord_tokens};
pub use inventory::ACTIONS;
pub use rebind::apply_live;

/// Re-exported for tests elsewhere in the crate that assert on a default
/// chord's rendering — see the const's own docs.
#[cfg(test)]
pub(crate) use format::SECONDARY_GLYPH;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

use gpui::{App, KeyBinding, Keystroke};

/// One registry entry. `bind` is a non-capturing fn so the table can be a
/// `const`; it panics on an unparsable chord, so callers must normalize
/// through [`normalize_chord`] first.
pub struct ActionSpec {
    /// Stable snake_case id — the `keybindings.toml` key.
    pub id: &'static str,
    /// Human label shown in the settings pane and search.
    pub label: &'static str,
    pub category: Category,
    /// Default chord in gpui syntax; "" = unbound by default.
    pub default_chord: &'static str,
    /// Build a [`KeyBinding`] for this action at the given chord.
    pub bind: fn(&str) -> KeyBinding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Global,
    Tabs,
    Panes,
    Terminal,
    Scm,
    Navigation,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Global,
        Category::Tabs,
        Category::Panes,
        Category::Terminal,
        Category::Scm,
        Category::Navigation,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Global => "Global",
            Category::Tabs => "Tabs",
            Category::Panes => "Panes & Layout",
            Category::Terminal => "Terminal & Agents",
            Category::Scm => "Source Control",
            Category::Navigation => "Navigation",
        }
    }
}

/// Effective chord per action id (`None` = unbound), in the canonical
/// `Keystroke::unparse` form so equality checks are order-insensitive.
pub type EffectiveMap = HashMap<&'static str, Option<String>>;

/// Result of merging overrides over defaults: the effective map plus any
/// human-readable problems found in the overrides (unknown ids, bad chords).
pub struct ResolveOutcome {
    pub effective: EffectiveMap,
    pub warnings: Vec<String>,
}

/// Parse + canonicalize a chord ("cmd-shift-t", multi-stroke "cmd-k cmd-b").
/// `None` when any stroke fails to parse, names an unknown key, or the
/// chord is blank. The key check matters because gpui's `Keystroke::parse`
/// accepts ANY trailing token as a key name — "cmd-notakey" would bind
/// silently and simply never match.
pub fn normalize_chord(chord: &str) -> Option<String> {
    let strokes: Vec<String> = chord
        .split_whitespace()
        .map(|s| {
            Keystroke::parse(s)
                .ok()
                .filter(|k| is_known_key(&k.key))
                .map(|k| k.unparse())
        })
        .collect::<Option<_>>()?;
    if strokes.is_empty() {
        return None;
    }
    Some(strokes.join(" "))
}

/// Keys a binding can name: any single character, the named editing/nav
/// keys, or a function key.
fn is_known_key(key: &str) -> bool {
    if key.chars().count() == 1 {
        return true;
    }
    if let Some(n) = key.strip_prefix('f')
        && let Ok(n) = n.parse::<u8>()
    {
        return (1..=19).contains(&n);
    }
    matches!(
        key,
        "enter"
            | "escape"
            | "tab"
            | "space"
            | "backspace"
            | "delete"
            | "up"
            | "down"
            | "left"
            | "right"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "insert"
    )
}

pub fn spec(id: &str) -> Option<&'static ActionSpec> {
    ACTIONS.iter().find(|s| s.id == id)
}

/// Merge `overrides` over the inventory defaults. An override that names an
/// unknown action or carries an unparsable chord is dropped with a warning
/// (the default stays live) — a typo must never cost a working shortcut.
pub fn resolve(overrides: &BTreeMap<String, String>) -> ResolveOutcome {
    let mut warnings = Vec::new();
    let mut effective: EffectiveMap = ACTIONS
        .iter()
        .map(|s| (s.id, normalize_chord(s.default_chord)))
        .collect();

    for (id, chord) in overrides {
        let Some(spec) = spec(id) else {
            warnings.push(format!("keybindings.toml: unknown action id `{id}`"));
            continue;
        };
        if chord.is_empty() {
            effective.insert(spec.id, None);
            continue;
        }
        match normalize_chord(chord) {
            Some(normalized) => {
                effective.insert(spec.id, Some(normalized));
            }
            None => warnings.push(format!(
                "keybindings.toml: invalid chord `{chord}` for `{id}` — keeping the default"
            )),
        }
    }
    ResolveOutcome {
        effective,
        warnings,
    }
}

fn bindings_for(effective: &EffectiveMap) -> Vec<KeyBinding> {
    ACTIONS
        .iter()
        .filter_map(|spec| {
            let chord = effective.get(spec.id)?.as_deref()?;
            Some((spec.bind)(chord))
        })
        .collect()
}

/// The pristine default keymap — used by the keymap-driven E2E tests so a
/// simulated keystroke exercises the same map the binary installs.
pub fn default_bindings() -> Vec<KeyBinding> {
    bindings_for(&resolve(&BTreeMap::new()).effective)
}

/// Process-wide effective map, seeded with defaults so chord display works
/// before (and without) `install` — e.g. in unit tests.
fn state() -> &'static RwLock<EffectiveMap> {
    static EFFECTIVE: OnceLock<RwLock<EffectiveMap>> = OnceLock::new();
    EFFECTIVE.get_or_init(|| RwLock::new(resolve(&BTreeMap::new()).effective))
}

fn store_effective(map: EffectiveMap) {
    let mut guard = match state().write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = map;
}

fn read_effective() -> EffectiveMap {
    match state().read() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// The live chord for an action id, canonical form. `None` = unbound or
/// unknown id.
pub fn chord_for(id: &str) -> Option<String> {
    match state().read() {
        Ok(g) => g.get(id).cloned().flatten(),
        Err(poisoned) => poisoned.into_inner().get(id).cloned().flatten(),
    }
}

/// The live chord formatted for display ("⌘⇧T"), or `None` when unbound.
pub fn display_chord_for(id: &str) -> Option<String> {
    chord_for(id).map(|c| format_chord(&c))
}

/// Per-glyph display tokens for the live chord (["⌘", "⇧", "T"]) — for UI
/// that renders one chip per key.
pub fn display_tokens_for(id: &str) -> Option<Vec<String>> {
    chord_for(id).map(|c| format_chord_tokens(&c))
}

/// Chords claimed by two or more actions in `effective` — every owner of a
/// returned chord should render a conflict badge. Same-context (global)
/// duplicates are legal in gpui (later wins) but always a user mistake.
pub fn conflicting_chords(effective: &EffectiveMap) -> HashSet<String> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for chord in effective.values().flatten() {
        *seen.entry(chord.as_str()).or_default() += 1;
    }
    seen.into_iter()
        .filter(|(_, n)| *n >= 2)
        .map(|(c, _)| c.to_string())
        .collect()
}

/// Boot install: bind the effective map and remember it for diffs +
/// display. Returns the override problems for the caller to surface.
pub fn install(cx: &mut App, overrides: &BTreeMap<String, String>) -> Vec<String> {
    let outcome = resolve(overrides);
    cx.bind_keys(bindings_for(&outcome.effective));
    store_effective(outcome.effective);
    outcome.warnings
}

#[cfg(test)]
mod tests;
