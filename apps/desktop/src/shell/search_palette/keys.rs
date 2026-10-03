//! Palette-scoped key bindings: `Tab` → project filter, `⌘1…⌘9` → quick
//! select. Arrows / Enter / Esc are the query input's own actions, caught at
//! the capture phase in the view (same contract as the Command Palette).

use gpui::{App, KeyBinding};

use crate::actions::{SearchPaletteFocusFilter, SearchPaletteQuickSelect};

/// Key context on the palette root while it is open.
pub const SEARCH_PALETTE_KEY_CONTEXT: &str = "SearchPalette";
/// The focused query input inside it: a binding scoped to the palette alone
/// is shallower than the input's own and loses; matching at the input's depth
/// ties, and the later registration (ours, after `gpui_component::init`) wins.
const SEARCH_PALETTE_INPUT_KEY_CONTEXT: &str = "SearchPalette > Input";

/// Install once at boot beside the other context-scoped sets, so none of
/// these chords shadow anything outside the palette.
pub fn register_search_palette_key_bindings(cx: &mut App) {
    let mut bindings = Vec::new();
    for context in [SEARCH_PALETTE_KEY_CONTEXT, SEARCH_PALETTE_INPUT_KEY_CONTEXT] {
        bindings.push(KeyBinding::new("tab", SearchPaletteFocusFilter, Some(context)));
        for digit in 1..=9u8 {
            bindings.push(KeyBinding::new(
                &format!("secondary-{digit}"),
                SearchPaletteQuickSelect { digit },
                Some(context),
            ));
        }
    }
    cx.bind_keys(bindings);
}
