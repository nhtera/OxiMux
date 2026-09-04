//! The one answer to "which agents does this app know about".
//!
//! Two surfaces need that answer and used to compute it separately: the
//! launcher's adapter picker walked the registry and `ACP_PRESETS` inline, and
//! the Agents settings pane carried a hard-coded array of four ids. The second
//! list is why `Custom` and every ACP agent had no environment editor and no
//! profiles, even though `env_for` and `profile_entry_mut` resolve for any
//! adapter id at all.
//!
//! [`agent_catalog`] composes the set once. Each surface still applies its own
//! *display* filter — the launcher hides `Custom` and anything the user
//! disabled, settings hides nothing because settings is where you re-enable it
//! — but neither decides membership any more.

use gpui::SharedString;
use oximux_agents::registry::RegistryEntry;
use oximux_settings::{AgentLaunchSettings, Transport};

/// The adapter list, and whether its `available` flags mean anything yet.
///
/// A bare `&[RegistryEntry]` cannot say the difference, and the difference
/// matters: `entries_without_detection` marks every adapter `available: true`,
/// so reading it as an answer would claim uninstalled agents are installed —
/// while treating a pending list as "unavailable" accuses every agent of being
/// missing for the first frames of every open.
#[derive(Clone, Copy, Debug)]
pub enum AdapterDetection<'a> {
    /// Detection has not answered; only the ids and names are usable.
    Pending(&'a [RegistryEntry]),
    /// Detection answered — `available` on each entry is meaningful.
    Done(&'a [RegistryEntry]),
}

impl<'a> AdapterDetection<'a> {
    fn entries(self) -> &'a [RegistryEntry] {
        match self {
            Self::Pending(e) | Self::Done(e) => e,
        }
    }

    fn availability(self, e: &RegistryEntry) -> Option<bool> {
        match self {
            Self::Pending(_) => None,
            Self::Done(_) => Some(e.available),
        }
    }
}

/// Where a catalog entry came from. Determines more than provenance: an ACP
/// agent's flags and model are decided by the ACP backend, not by
/// `agent_launch.toml`, so the settings pane must not offer controls that
/// write values nothing will read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentOrigin {
    /// A registered `CliAgentAdapter` — the built-in four plus `Custom`.
    /// Launches with `args_for_in` / `model_for_in` applied.
    Builtin,
    /// A zero-config entry from [`oximux_settings::ACP_PRESETS`].
    AcpPreset,
    /// An `[agents.<id>]` block the user wrote with `transport = "acp"`, for an
    /// id that is neither a built-in nor a preset.
    ConfiguredAcp,
}

impl AgentOrigin {
    /// Whether this agent's launch reads `args` and `model` from
    /// `agent_launch.toml`.
    ///
    /// False for both ACP flavours: the chat backend is built from
    /// `acp_command` + `acp_args`, and `env_for` is the only one of the three
    /// profile axes that reaches the process. A skip-perms chip on one of these
    /// would write a flag no spawn ever reads.
    pub fn takes_flags_and_model(self) -> bool {
        matches!(self, Self::Builtin)
    }
}

/// One agent the app knows how to configure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogAgent {
    /// The id every settings accessor is keyed on — an adapter's `id()`, a
    /// preset's `id`, or the `[agents.<id>]` table name.
    pub id: SharedString,
    pub display: SharedString,
    pub origin: AgentOrigin,
    /// Whether the underlying binary was found on PATH. `None` means detection
    /// has not answered yet — rendered neutral, never as "not installed",
    /// because the first frames of every open would otherwise accuse every
    /// agent of being missing.
    pub available: Option<bool>,
}

/// Compose the full agent set: registry adapters, then the ACP presets, then
/// any ACP agent the user configured by hand.
///
/// `preset_available` is parallel to [`oximux_settings::ACP_PRESETS`], the same
/// convention the launcher's picker uses; `None` before its detection has run.
///
/// Order is stable and meaningful: registration order for the built-ins (which
/// is the order the launcher lists them in), declaration order for the presets,
/// and id order for configured entries — they come from a `BTreeMap`, so a
/// hand-edited file cannot reshuffle the pane.
pub fn agent_catalog(
    adapters: AdapterDetection<'_>,
    preset_available: Option<&[bool]>,
    launch: &AgentLaunchSettings,
) -> Vec<CatalogAgent> {
    let mut out: Vec<CatalogAgent> = Vec::new();

    for e in adapters.entries() {
        out.push(CatalogAgent {
            id: SharedString::from(e.adapter_id),
            display: SharedString::from(e.display_name),
            origin: AgentOrigin::Builtin,
            available: adapters.availability(e),
        });
    }

    // Built-in ACP presets.
    for (ix, preset) in oximux_settings::ACP_PRESETS.iter().enumerate() {
        // A preset id that a registered adapter already claims is that
        // adapter, not a preset — membership is decided once, by the first
        // source that claims the id.
        if out.iter().any(|a| a.id == preset.id) {
            continue;
        }
        out.push(CatalogAgent {
            id: SharedString::from(preset.id),
            display: SharedString::from(preset.title),
            origin: AgentOrigin::AcpPreset,
            available: preset_available.and_then(|a| a.get(ix).copied()),
        });
    }

    // Hand-configured ACP agents: an `[agents.<id>]` block that declares the
    // ACP transport for an id nothing above claimed. These have never appeared
    // in any picker; settings is where a user would go looking for them.
    for (id, entry) in &launch.agents {
        if entry.transport != Transport::Acp {
            continue;
        }
        if out.iter().any(|a| a.id.as_ref() == id.as_str()) {
            continue;
        }
        out.push(CatalogAgent {
            id: SharedString::from(id.clone()),
            display: SharedString::from(id.clone()),
            origin: AgentOrigin::ConfiguredAcp,
            // Its command is user-supplied and not probed anywhere, so nothing
            // can honestly claim it is or is not installed.
            available: None,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::registry::AdapterRegistry;
    use oximux_settings::PerAgentLaunch;

    fn registry() -> AdapterRegistry {
        AdapterRegistry::with_builtin_adapters()
    }

    #[test]
    fn the_catalog_covers_every_agent_the_app_can_launch() {
        let reg = registry();
        let launch = AgentLaunchSettings::default();
        let registered = reg.entries_without_detection();
        let cat = agent_catalog(AdapterDetection::Pending(&registered), None, &launch);

        // Every registered adapter, `Custom` included — the exclusion that kept
        // it out of the settings pane was a display filter, not membership.
        for kind in reg.builtin_kinds() {
            let adapter = reg.adapter_for(kind).expect("registered");
            assert!(
                cat.iter().any(|a| a.id == adapter.id()),
                "{} missing from the catalog",
                adapter.id(),
            );
        }
        // Every ACP preset.
        for p in oximux_settings::ACP_PRESETS {
            let found = cat.iter().find(|a| a.id == p.id).expect("preset in catalog");
            assert_eq!(found.origin, AgentOrigin::AcpPreset);
        }
        // Nothing is claimed to be installed before detection has answered.
        assert!(cat.iter().all(|a| a.available.is_none()));
    }

    #[test]
    fn a_hand_configured_acp_agent_appears_and_a_plain_one_does_not() {
        let reg = registry();
        let mut launch = AgentLaunchSettings::default();
        launch.agents.insert(
            "in-house".into(),
            PerAgentLaunch {
                transport: Transport::Acp,
                acp_command: "in-house-acp".into(),
                ..PerAgentLaunch::default()
            },
        );
        // A stream-json entry is a built-in's configuration, not a new agent.
        launch.agents.insert("claude-code".into(), PerAgentLaunch::default());

        let registered = reg.entries_without_detection();
        let cat = agent_catalog(AdapterDetection::Pending(&registered), None, &launch);
        let found = cat.iter().find(|a| a.id == "in-house").expect("configured ACP agent");
        assert_eq!(found.origin, AgentOrigin::ConfiguredAcp);
        assert_eq!(found.available, None, "its command is user-supplied and never probed");
        // The built-in keeps its own origin rather than being duplicated by its
        // own settings entry.
        assert_eq!(cat.iter().filter(|a| a.id == "claude-code").count(), 1);
        assert_eq!(
            cat.iter().find(|a| a.id == "claude-code").expect("built-in").origin,
            AgentOrigin::Builtin,
        );
    }

    #[test]
    fn detection_is_matched_by_id_not_by_position() {
        let reg = registry();
        let launch = AgentLaunchSettings::default();
        // A detected list in a different order than registration, and one entry
        // short — the shape a registry change between the two calls produces.
        let mut detected = reg.entries_without_detection();
        detected.reverse();
        detected.truncate(detected.len() - 1);
        for e in &mut detected {
            e.available = false;
        }
        let cat = agent_catalog(AdapterDetection::Done(&detected), None, &launch);

        for e in &detected {
            let found = cat.iter().find(|a| a.id == e.adapter_id).expect("in catalog");
            assert_eq!(found.available, Some(false));
        }
        // Presets still report unknown, because their own detection did not run.
        assert!(
            cat.iter()
                .filter(|a| a.origin == AgentOrigin::AcpPreset)
                .all(|a| a.available.is_none())
        );
    }

    #[test]
    fn preset_availability_is_read_positionally_the_way_the_launcher_reports_it() {
        let reg = registry();
        let launch = AgentLaunchSettings::default();
        let avail: Vec<bool> =
            (0..oximux_settings::ACP_PRESETS.len()).map(|i| i % 2 == 0).collect();
        let registered = reg.entries_without_detection();
        let cat = agent_catalog(AdapterDetection::Pending(&registered), Some(&avail), &launch);
        for (ix, p) in oximux_settings::ACP_PRESETS.iter().enumerate() {
            let found = cat.iter().find(|a| a.id == p.id).expect("preset");
            assert_eq!(found.available, Some(avail[ix]), "{} availability", p.id);
        }
    }

    /// The defect this phase closes, stated as an invariant: the launcher and
    /// the settings pane must not have separate notions of which agents exist.
    ///
    /// One direction is the meaningful one. Settings shows everything, because
    /// settings is where you configure an agent you disabled or have not
    /// installed; the launcher shows a subset. What must never happen is the
    /// launcher offering something settings cannot configure — that is exactly
    /// what a hard-coded four-item list produced.
    #[test]
    fn nothing_the_launcher_offers_is_missing_from_the_settings_catalog() {
        use crate::shell::terminal::adapter_picker::render_rows_for_test;

        let reg = AdapterRegistry::with_builtin_adapters();
        let mut launch =
            AgentLaunchSettings { default_agent: "codex".into(), ..AgentLaunchSettings::default() };
        // A hand-written ACP agent and a disabled built-in, so the two lists
        // are asked about the interesting cases and not just the easy ones.
        launch.agents.insert(
            "in-house".into(),
            PerAgentLaunch { transport: Transport::Acp, ..PerAgentLaunch::default() },
        );
        launch.agents.insert(
            "pi".into(),
            PerAgentLaunch { disabled: true, ..PerAgentLaunch::default() },
        );

        let detected = reg.entries_without_detection();
        let catalog = agent_catalog(AdapterDetection::Done(&detected), None, &launch);

        for row in render_rows_for_test(&detected, &launch) {
            assert!(
                catalog.iter().any(|c| c.id == row.adapter_id),
                "the launcher offers {} but settings cannot configure it",
                row.adapter_id,
            );
        }
        for preset in oximux_settings::ACP_PRESETS {
            assert!(
                catalog.iter().any(|c| c.id == preset.id),
                "the launcher can start the {} preset but settings cannot configure it",
                preset.id,
            );
        }
        // And the widening actually happened: the two the old list could not
        // reach are both here.
        assert!(catalog.iter().any(|c| c.id == "custom"));
        assert!(catalog.iter().any(|c| c.id == "in-house"));
        // A disabled agent is still configurable — settings is where it gets
        // re-enabled, so hiding it there would be a trap.
        assert!(catalog.iter().any(|c| c.id == "pi"));
    }

    #[test]
    fn only_a_builtin_takes_flags_and_a_model() {
        // The ACP chat backend is built from `acp_command` + `acp_args`; `env`
        // is the only profile axis that reaches it. Offering the other two
        // would write values no spawn reads.
        assert!(AgentOrigin::Builtin.takes_flags_and_model());
        assert!(!AgentOrigin::AcpPreset.takes_flags_and_model());
        assert!(!AgentOrigin::ConfiguredAcp.takes_flags_and_model());
    }
}
