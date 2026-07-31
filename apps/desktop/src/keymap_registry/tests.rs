//! Pure-logic tests for the registry: chord normalization + formatting
//! round-trips, override precedence, conflict detection, and inventory
//! invariants (unique ids, parsable defaults).

use std::collections::BTreeMap;

use super::*;

fn overrides(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn inventory_ids_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for spec in ACTIONS {
        assert!(seen.insert(spec.id), "duplicate action id `{}`", spec.id);
    }
}

#[test]
fn inventory_default_chords_all_parse() {
    for spec in ACTIONS {
        if spec.default_chord.is_empty() {
            continue;
        }
        assert!(
            normalize_chord(spec.default_chord).is_some(),
            "default chord `{}` for `{}` does not parse",
            spec.default_chord,
            spec.id
        );
    }
}

#[test]
fn inventory_default_chords_do_not_conflict() {
    let effective = resolve(&BTreeMap::new()).effective;
    let conflicts = conflicting_chords(&effective);
    assert!(
        conflicts.is_empty(),
        "default keymap has conflicting chords: {conflicts:?}"
    );
}

#[test]
fn normalize_is_order_insensitive() {
    assert_eq!(
        normalize_chord("shift-cmd-t"),
        normalize_chord("cmd-shift-t")
    );
}

#[test]
fn normalize_handles_multi_stroke() {
    let n = normalize_chord("cmd-k  cmd-b").expect("parses");
    assert_eq!(n.split(' ').count(), 2);
}

#[test]
fn normalize_rejects_garbage() {
    assert_eq!(normalize_chord("cmd-notakey"), None);
    assert_eq!(normalize_chord(""), None);
    assert_eq!(normalize_chord("   "), None);
}

#[test]
fn override_replaces_default() {
    let out = resolve(&overrides(&[("new_tab", "cmd-y")]));
    assert_eq!(
        out.effective.get("new_tab").unwrap().as_deref(),
        normalize_chord("cmd-y").as_deref()
    );
    assert!(out.warnings.is_empty());
}

#[test]
fn empty_override_unbinds() {
    let out = resolve(&overrides(&[("dismiss_overlay", "")]));
    assert_eq!(out.effective.get("dismiss_overlay").unwrap(), &None);
}

#[test]
fn invalid_chord_keeps_default_and_warns() {
    let out = resolve(&overrides(&[("new_tab", "cmd-notakey")]));
    // `secondary-`, not `cmd-`: this compares against the shipped default, and
    // the two normalize to the same modifier only on macOS. Written as `cmd-`
    // this assertion would keep passing here while silently ceasing to check
    // anything real on a platform where they differ.
    assert_eq!(
        out.effective.get("new_tab").unwrap().as_deref(),
        normalize_chord("secondary-t").as_deref()
    );
    assert_eq!(out.warnings.len(), 1);
    assert!(out.warnings[0].contains("new_tab"));
}

#[test]
fn unknown_action_id_warns() {
    let out = resolve(&overrides(&[("not_an_action", "cmd-y")]));
    assert_eq!(out.warnings.len(), 1);
    assert!(out.warnings[0].contains("not_an_action"));
}

#[test]
fn conflict_detection_flags_duplicate_chords() {
    // Move new_tab onto search_scrollback's default chord. `secondary-`, not
    // `cmd-`, for the reason spelled out in `override_with_unparseable_chord`
    // below: the collision is against a *shipped default*, and those are written
    // with `secondary-`. On Windows `cmd-f` normalizes to a literal Command
    // modifier that nothing else holds, so there was no conflict to find and the
    // test failed while the detector was working.
    let out = resolve(&overrides(&[("new_tab", "secondary-f")]));
    let conflicts = conflicting_chords(&out.effective);
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts.contains(normalize_chord("secondary-f").unwrap().as_str()));
}

#[test]
fn default_bindings_cover_every_bound_action() {
    let bound = ACTIONS
        .iter()
        .filter(|s| !s.default_chord.is_empty())
        .count();
    assert_eq!(default_bindings().len(), bound);
}

#[test]
fn format_chord_matches_shipped_glyph_convention() {
    assert_eq!(format_chord("cmd-shift-e"), "⌘⇧E");
    assert_eq!(format_chord("ctrl-shift-1"), "⌃⇧1");
    assert_eq!(format_chord("ctrl-tab"), "⌃Tab");
    assert_eq!(format_chord("cmd-shift-enter"), "⌘⇧↩");
    assert_eq!(format_chord("escape"), "Esc");
    assert_eq!(format_chord("cmd-,"), "⌘,");
    assert_eq!(format_chord("cmd-}"), "⌘}");
    assert_eq!(format_chord("cmd-alt-left"), "⌘⌥←");
}

#[test]
fn format_chord_handles_multi_stroke() {
    assert_eq!(format_chord("cmd-k cmd-b"), "⌘K ⌘B");
}

#[test]
fn format_tokens_split_modifiers_and_key() {
    assert_eq!(format_chord_tokens("cmd-shift-n"), vec!["⌘", "⇧", "N"]);
    assert_eq!(format_chord_tokens("cmd-o"), vec!["⌘", "O"]);
}

mod rebind_plan {
    use std::collections::BTreeMap;

    use crate::keymap_registry::rebind::{RebindStep, plan_rebind};
    use crate::keymap_registry::{EffectiveMap, normalize_chord, resolve, spec};

    fn effective(pairs: &[(&str, &str)]) -> EffectiveMap {
        let mut map = resolve(&BTreeMap::new()).effective;
        for (id, chord) in pairs {
            let spec_id = spec(id).expect("known id").id;
            let value = (!chord.is_empty()).then(|| normalize_chord(chord).expect("parses"));
            map.insert(spec_id, value);
        }
        map
    }

    fn bind_index(steps: &[RebindStep], id: &str, chord: &str) -> usize {
        let chord = normalize_chord(chord).unwrap();
        steps
            .iter()
            .position(|s| matches!(s, RebindStep::Bind(i, c) if *i == id && *c == chord))
            .unwrap_or_else(|| panic!("no Bind({id}, {chord}) in {steps:?}"))
    }

    fn shadow_index(steps: &[RebindStep], chord: &str) -> usize {
        let chord = normalize_chord(chord).unwrap();
        steps
            .iter()
            .position(|s| matches!(s, RebindStep::Shadow(c) if *c == chord))
            .unwrap_or_else(|| panic!("no Shadow({chord}) in {steps:?}"))
    }

    #[test]
    fn no_change_produces_empty_plan() {
        let map = effective(&[]);
        assert!(plan_rebind(&map, &map).is_empty());
    }

    // These plans are compared against *shipped defaults*, which are written
    // with `secondary-`. `normalize_chord` unparses to the concrete modifier —
    // `cmd-t` on macOS, `ctrl-t` on Windows — so a `cmd-` literal here named a
    // chord no default held off macOS, and the needle matched nothing. The
    // `bind_index`/`shadow_index` helpers normalize what they are given, so
    // `secondary-` is all that is needed.
    #[test]
    fn moved_chord_is_shadowed_then_bound() {
        let prev = effective(&[]);
        let next = effective(&[("new_tab", "secondary-y")]);
        let steps = plan_rebind(&prev, &next);
        // Old chord dies, new chord binds after its shadow (if any).
        assert!(shadow_index(&steps, "secondary-t") < steps.len());
        assert!(
            shadow_index(&steps, "secondary-y")
                < bind_index(&steps, "new_tab", "secondary-y")
        );
        // Nothing else owns new_tab's old chord in `next`, so it must NOT be
        // re-bound.
        let old = normalize_chord("secondary-t").expect("parses");
        assert!(
            !steps
                .iter()
                .any(|s| matches!(s, RebindStep::Bind(_, c) if *c == old))
        );
    }

    #[test]
    fn conflict_then_resolve_rebinds_the_surviving_owner() {
        // Step 1 gave new_tab search's chord (conflict); step 2 moves
        // search away to a free chord. The surviving owner (new_tab @
        // secondary-f) must be re-bound AFTER the secondary-f shadow, or the
        // shadow — appended after new_tab's step-1 binding — leaves it dead.
        let prev = effective(&[("new_tab", "secondary-f")]);
        let next = effective(&[("new_tab", "secondary-f"), ("search_scrollback", "secondary-9")]);
        let steps = plan_rebind(&prev, &next);
        assert!(
            shadow_index(&steps, "secondary-f")
                < bind_index(&steps, "new_tab", "secondary-f")
        );
        assert!(
            shadow_index(&steps, "secondary-9")
                < bind_index(&steps, "search_scrollback", "secondary-9")
        );
    }

    #[test]
    fn chord_swap_rebinds_both_owners_after_shadows() {
        let prev = effective(&[]);
        let next = effective(&[("new_tab", "cmd-f"), ("search_scrollback", "cmd-t")]);
        let steps = plan_rebind(&prev, &next);
        assert!(shadow_index(&steps, "cmd-t") < bind_index(&steps, "search_scrollback", "cmd-t"));
        assert!(shadow_index(&steps, "cmd-f") < bind_index(&steps, "new_tab", "cmd-f"));
    }

    #[test]
    fn unbind_shadows_without_rebinding() {
        let prev = effective(&[]);
        let next = effective(&[("dismiss_overlay", "")]);
        let steps = plan_rebind(&prev, &next);
        assert!(shadow_index(&steps, "escape") < steps.len());
        assert!(!steps.iter().any(|s| matches!(s, RebindStep::Bind(..))));
    }
}

#[test]
fn display_lookup_falls_back_to_defaults_without_install() {
    // No App in unit tests — the lazily-seeded default map must serve.
    // `open_quick_open`'s default is `secondary-p`, so the glyph is platform-
    // dependent; hardcoding ⌘ asserted macOS everywhere.
    let expected = format!("{SECONDARY_GLYPH}P");
    assert_eq!(
        display_chord_for("open_quick_open").as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(display_chord_for("reload_custom_commands"), None);
    assert_eq!(display_chord_for("no_such_action"), None);
}
