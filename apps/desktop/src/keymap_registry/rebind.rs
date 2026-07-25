//! Live rebinding — applied when the settings pane saves an override.
//!
//! GPUI matches bindings in reverse insertion order at equal context
//! depth, and the boot keymap can't be cleared (that would drop the
//! component library's text-input bindings), so a rebind APPENDS. For
//! every chord touched by the diff the plan first appends a [`NoAction`]
//! shadow (killing every older owner), then re-appends the binding of
//! every action that owns an affected chord in the NEW map — so the
//! current owner always ends up most recent. Shadowing alone is not
//! enough: when a chord stays owned by one action while another action
//! vacates it, the vacating action's old binding would otherwise still
//! outrank the surviving owner's.

use std::collections::{BTreeMap, BTreeSet};

use gpui::{App, KeyBinding, NoAction};

use super::{ACTIONS, EffectiveMap, read_effective, resolve, spec, store_effective};

/// One step of a rebind plan, in application order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RebindStep {
    /// Append a NoAction binding for this chord (kills older owners).
    Shadow(String),
    /// Append the binding `action id → chord` (becomes most recent).
    Bind(&'static str, String),
}

/// Pure diff: every chord that appears on a changed entry (old or new
/// side) gets a shadow, then every action owning an affected chord in
/// `next` gets re-bound. Returns an empty plan when nothing changed.
pub(crate) fn plan_rebind(prev: &EffectiveMap, next: &EffectiveMap) -> Vec<RebindStep> {
    let mut affected: BTreeSet<String> = BTreeSet::new();
    for spec in ACTIONS {
        let old = prev.get(spec.id).cloned().flatten();
        let new = next.get(spec.id).cloned().flatten();
        if old != new {
            affected.extend(old);
            affected.extend(new);
        }
    }

    let mut steps: Vec<RebindStep> = affected.iter().cloned().map(RebindStep::Shadow).collect();
    for spec in ACTIONS {
        if let Some(chord) = next.get(spec.id).and_then(|c| c.as_ref())
            && affected.contains(chord)
        {
            steps.push(RebindStep::Bind(spec.id, chord.clone()));
        }
    }
    steps
}

/// Live rebind after a settings edit. Returns the override problems for
/// the caller to surface.
pub fn apply_live(cx: &mut App, overrides: &BTreeMap<String, String>) -> Vec<String> {
    let outcome = resolve(overrides);
    let prev = read_effective();
    let bindings: Vec<KeyBinding> = plan_rebind(&prev, &outcome.effective)
        .into_iter()
        .filter_map(|step| match step {
            RebindStep::Shadow(chord) => Some(KeyBinding::new(&chord, NoAction, None)),
            RebindStep::Bind(id, chord) => Some((spec(id)?.bind)(&chord)),
        })
        .collect();
    if !bindings.is_empty() {
        cx.bind_keys(bindings);
    }
    store_effective(outcome.effective);
    outcome.warnings
}
