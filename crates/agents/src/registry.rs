//! Startup-time inventory of CLI agent adapters.
//!
//! Step 8 deliverable: bundle every built-in adapter, scan PATH once at
//! boot, and hand the launch-dialog UI a list of which agents the user
//! actually has installed. The registry stays decoupled from `CliRuntime`
//! so adapters can be enumerated for the dialog without forcing a runtime
//! to exist yet (the UI lives in a thread that hasn't built the runtime
//! when the dropdown first paints).
//!
//! ## Detection cost
//!
//! `detect()` shells out to `which <bin>` per adapter — roughly 15 ms
//! each, so the five adapters run ~75 ms wall-clock total at cold boot,
//! still well under the 200 ms felt threshold. Sequential stays
//! intentional — the `JoinSet` alternative remains premature until
//! cold-start telemetry shows detection crossing that threshold, at which
//! point swap the loop body for a `tokio::task::JoinSet` walk.
//!
//! ## Errors fold to unavailable
//!
//! A `detect()` `Err` is logged at WARN and treated as `available = false`.
//! Rationale: a single bad adapter must not block boot for the other
//! three. The most likely failure mode is `which` missing from PATH or
//! the spawn returning an I/O error — neither of which the user can
//! act on from a panic.

use std::sync::Arc;

use oximux_core::AgentAdapter;

use crate::cli::{
    ClaudeCodeAdapter, CliAgentAdapter, CodexAdapter, CustomCommandAdapter, OmpAdapter,
    PiAdapter,
};

/// One adapter entry the launch-dialog UI consumes. `&'static str` for
/// `adapter_id` and `display_name` because the underlying trait already
/// hands back static strings — keeping them static avoids per-entry
/// allocation when the dialog re-renders.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    /// Stable slug (`claude-code`, `codex`, `pi`, `custom`). Matches
    /// `CliAgentAdapter::id()`. Storage uses this discriminator in Phase 4.
    /// `'static` is contractually backed by [`CliAgentAdapter::id`]'s
    /// return type. If a future trait revision relaxes that lifetime (for
    /// example to let `Custom` carry a user-named preset), switch this
    /// field to `String` — the compiler will surface every call site.
    pub adapter_id: &'static str,
    /// Human label for the dropdown. Same `'static` rationale as
    /// `adapter_id` above.
    pub display_name: &'static str,
    /// The domain enum the runtime keys sessions on.
    pub adapter_enum: AgentAdapter,
    /// `true` when the underlying binary is on PATH (or the adapter is
    /// always-on, like `Custom`). `false` when `detect()` returned `false`
    /// OR returned `Err` (logged at WARN; see module doc).
    pub available: bool,
    /// Concrete model selectors the launch picker offers (adapter-declared,
    /// `&'static`). Empty → no model row. The picker prepends a "Default"
    /// option that maps to no `--model` flag.
    pub models: &'static [&'static str],
    /// Reasoning-effort selectors the picker offers. Empty → no effort row.
    pub efforts: &'static [&'static str],
}

/// Internal pair: the runtime needs the `AgentAdapter` enum to key its
/// session table, the trait surfaces `id()` only. Co-locating both lets
/// the registry hand the runtime either lookup at no extra cost.
struct AdapterSlot {
    kind: AgentAdapter,
    adapter: Arc<dyn CliAgentAdapter>,
}

/// Startup inventory. Held by the app behind whatever lock pattern fits
/// (current plan: built once at boot, never mutated). `Send + Sync`
/// inherited from the trait bound on `CliAgentAdapter`.
pub struct AdapterRegistry {
    slots: Vec<AdapterSlot>,
}

impl AdapterRegistry {
    /// Registry preloaded with the built-in adapters in launch-dialog order:
    /// Claude Code, Codex, Pi, Custom. Order is significant — the dialog
    /// dropdown renders in this order; the chat-capable adapters lead, and
    /// `Custom` sits last so the escape hatch is where users learn to expect it.
    ///
    /// This registry is also what the **chat** roster enumerates, so an adapter
    /// missing here is unreachable as a chat backend no matter what
    /// `AgentLaunchSettings::chat_capable` says about it — that function is only
    /// ever asked about ids these entries produce.
    pub fn with_builtin_adapters() -> Self {
        let mut reg = Self::empty();
        reg.register(AgentAdapter::ClaudeCode, Arc::new(ClaudeCodeAdapter));
        reg.register(AgentAdapter::Codex, Arc::new(CodexAdapter));
        reg.register(AgentAdapter::Pi, Arc::new(PiAdapter));
        reg.register(AgentAdapter::Omp, Arc::new(OmpAdapter));
        reg.register(AgentAdapter::Custom, Arc::new(CustomCommandAdapter));
        reg
    }

    /// Empty registry. The only use case is tests that need to register
    /// a mock adapter without the built-in four cluttering assertions.
    pub fn empty() -> Self {
        Self { slots: Vec::new() }
    }

    /// Append one `(kind, adapter)` pair. Last-write-wins is NOT enforced
    /// — a duplicate enum will be appended again and `adapter_for` will
    /// return the first match. Tests that swap a mock for a built-in
    /// should construct from `empty()` rather than push onto a built-in
    /// registry.
    ///
    /// NOTE: this diverges from `CliRuntime::register_adapter`, which is
    /// `HashMap::insert`-backed and therefore last-write-wins. Same nominal
    /// operation, opposite collision behaviour — keep the registry side
    /// strictly one-`register`-per-kind so the two stay aligned at the
    /// `adapter_for` boundary.
    pub fn register(&mut self, kind: AgentAdapter, adapter: Arc<dyn CliAgentAdapter>) {
        self.slots.push(AdapterSlot { kind, adapter });
    }

    /// Number of registered adapters.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// `true` when no adapters are registered.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Lookup by domain enum. Returns the first match; with the built-in
    /// registry every kind is registered exactly once so "first match"
    /// equals "the match".
    pub fn adapter_for(&self, kind: AgentAdapter) -> Option<Arc<dyn CliAgentAdapter>> {
        self.slots
            .iter()
            .find(|s| s.kind == kind)
            .map(|s| s.adapter.clone())
    }

    /// Lookup by `id()` slug. Used by future settings overrides and by
    /// storage replay (Phase 4) when re-hydrating sessions by their
    /// persisted adapter discriminator.
    pub fn adapter_by_id(&self, id: &str) -> Option<Arc<dyn CliAgentAdapter>> {
        self.slots
            .iter()
            .find(|s| s.adapter.id() == id)
            .map(|s| s.adapter.clone())
    }

    /// Run `detect()` on every registered adapter, in registration order,
    /// and return one `RegistryEntry` per slot. See module doc for the
    /// sequential-vs-parallel and error-folding rationale.
    ///
    /// No wall-clock cap is enforced here. If `which` hangs on a slow PATH
    /// mount (NFS, networked $HOME) the whole walk stalls. Callers that
    /// need a hard upper bound should wrap:
    ///
    /// ```ignore
    /// tokio::time::timeout(Duration::from_millis(500), reg.detect_available()).await
    /// ```
    pub async fn detect_available(&self) -> Vec<RegistryEntry> {
        let mut entries = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            let adapter_id = slot.adapter.id();
            let display_name = slot.adapter.name();
            let available = match slot.adapter.detect().await {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(
                        adapter = adapter_id,
                        %err,
                        "detect failed; treating adapter as unavailable",
                    );
                    false
                }
            };
            entries.push(RegistryEntry {
                adapter_id,
                display_name,
                adapter_enum: slot.kind,
                available,
                models: slot.adapter.models(),
                efforts: slot.adapter.efforts(),
            });
        }
        entries
    }

    /// Every registered adapter's kind, in registration order.
    ///
    /// Exists so a consumer that must act on all of them (the runtime, which
    /// registers each for `start_session` to resolve) can *derive* the set
    /// instead of restating it. A restated list drifts the moment an adapter is
    /// added, and drifts invisibly: the new agent renders in every menu, its
    /// click routes, and only `start_session` fails — into a log.
    pub fn builtin_kinds(&self) -> Vec<AgentAdapter> {
        self.slots.iter().map(|s| s.kind).collect()
    }

    /// One [`RegistryEntry`] per registered adapter WITHOUT running `detect()`,
    /// every entry marked `available: true`. For synchronous, pre-launch
    /// consumers (like the unified chat roster) that need each adapter's id,
    /// display name, and static model/effort vocab but do NOT gate on whether
    /// the binary is on PATH — the composer's agent picker offers the choice
    /// regardless, and the spawn attempt surfaces a missing binary. Mirrors
    /// [`Self::detect_available`] minus the async which-detection.
    pub fn entries_without_detection(&self) -> Vec<RegistryEntry> {
        self.slots
            .iter()
            .map(|slot| RegistryEntry {
                adapter_id: slot.adapter.id(),
                display_name: slot.adapter.name(),
                adapter_enum: slot.kind,
                available: true,
                models: slot.adapter.models(),
                efforts: slot.adapter.efforts(),
            })
            .collect()
    }
}

// No `Default` impl on purpose. Rust convention says `Default` is the
// cheapest sensible state; making it `with_builtin_adapters()` would
// silently construct four real adapters under any future
// `let reg: AdapterRegistry = Default::default()`, and making it `empty()`
// would surprise callers expecting a populated registry. Both call sites
// are already explicit (`with_builtin_adapters()` / `empty()`) so the
// trait carries no weight.

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::cli::{CommandSpec, StatusPattern};
    use crate::runtime::AgentSessionConfig;

    /// Mock that reports a configurable `detect` result and counts how
    /// often `detect` is invoked — used to lock both the "once per
    /// registry call" contract and to play a clean "available second
    /// entry" alongside `ErroringMock`.
    struct CountingMock {
        id: &'static str,
        result: bool,
        calls: AtomicUsize,
    }

    impl CountingMock {
        fn new(id: &'static str, result: bool) -> Self {
            Self {
                id,
                result,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl CliAgentAdapter for CountingMock {
        fn id(&self) -> &'static str {
            self.id
        }
        fn name(&self) -> &'static str {
            // Same string is fine for tests — the trait only requires
            // 'static and the test assertions never compare names.
            "Mock"
        }
        async fn detect(&self) -> Result<bool> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.result)
        }
        fn build_command(&self, _cfg: &AgentSessionConfig) -> Result<CommandSpec> {
            unimplemented!("mock is detect-only")
        }
        fn status_patterns(&self) -> &[StatusPattern] {
            &[]
        }
    }

    /// Mock whose `detect()` returns `Err` — exercises the warn-and-fold
    /// path in `detect_available`.
    struct ErroringMock;

    #[async_trait]
    impl CliAgentAdapter for ErroringMock {
        fn id(&self) -> &'static str {
            "erroring"
        }
        fn name(&self) -> &'static str {
            "Erroring"
        }
        async fn detect(&self) -> Result<bool> {
            Err(anyhow::anyhow!("synthetic detect failure"))
        }
        fn build_command(&self, _cfg: &AgentSessionConfig) -> Result<CommandSpec> {
            unimplemented!("mock is detect-only")
        }
        fn status_patterns(&self) -> &[StatusPattern] {
            &[]
        }
    }

    #[test]
    fn empty_registry_has_zero_adapters() {
        let reg = AdapterRegistry::empty();
        assert_eq!(reg.len(), 0);
        assert!(reg.is_empty());
    }

    #[test]
    fn builtin_registry_holds_every_adapter_in_dialog_order() {
        let reg = AdapterRegistry::with_builtin_adapters();
        assert_eq!(reg.len(), 5);
        // Order contract — the launch dialog renders in this sequence.
        let order: Vec<AgentAdapter> = reg.slots.iter().map(|s| s.kind).collect();
        assert_eq!(
            order,
            vec![
                AgentAdapter::ClaudeCode,
                AgentAdapter::Codex,
                AgentAdapter::Pi,
                AgentAdapter::Omp,
                AgentAdapter::Custom,
            ],
            "dropdown order is significant; Custom must remain last"
        );
    }

    /// Every chat backend must have a registry entry, because the chat roster is
    /// built from this list — an adapter missing here is unreachable in the UI no
    /// matter how complete its backend is, and `chat_capable` will never be asked
    /// about it. Pi shipped in exactly that state until this was caught by
    /// driving the real app.
    #[test]
    fn every_chat_capable_adapter_is_registered() {
        let ids: Vec<&str> = AdapterRegistry::with_builtin_adapters()
            .entries_without_detection()
            .iter()
            .map(|e| e.adapter_id)
            .collect();
        for chat_id in ["claude-code", "codex", "pi"] {
            assert!(
                ids.contains(&chat_id),
                "{chat_id} is chat-capable but absent from the registry, so no menu can offer it"
            );
        }
    }

    /// `builtin_kinds` is what the runtime registers from, so it must cover every
    /// slot. When it didn't, Pi rendered in the menu, its click routed, and the
    /// launch died with "no adapter registered for Pi" in a log — nothing at all
    /// happened on screen.
    #[test]
    fn builtin_kinds_covers_every_registered_slot() {
        let reg = AdapterRegistry::with_builtin_adapters();
        let kinds = reg.builtin_kinds();
        assert_eq!(kinds.len(), reg.len(), "a kind per slot, or the runtime misses one");
        for kind in kinds {
            assert!(reg.adapter_for(kind).is_some(), "{kind:?} resolves to an adapter");
        }
    }

    #[test]
    fn entries_without_detection_reports_every_builtin_as_available() {
        let reg = AdapterRegistry::with_builtin_adapters();
        let entries = reg.entries_without_detection();
        // One per slot, in registration order, all forced available (no detect).
        let ids: Vec<&str> = entries.iter().map(|e| e.adapter_id).collect();
        assert_eq!(ids, ["claude-code", "codex", "pi", "omp", "custom"]);
        assert!(entries.iter().all(|e| e.available));
        // Static vocab rides along (the roster's pre-bind model source).
        let claude = entries.iter().find(|e| e.adapter_id == "claude-code").unwrap();
        assert_eq!(claude.models, ["opus", "fable", "sonnet", "haiku"]);
        // Pi declares none: its catalog is per-user, and a static list would have
        // to name bare ids, which pi fuzzy-matches across providers.
        let pi = entries.iter().find(|e| e.adapter_id == "pi").unwrap();
        assert!(pi.models.is_empty(), "a static bare-id list would misresolve");
    }

    #[test]
    fn adapter_for_returns_each_builtin_kind() {
        let reg = AdapterRegistry::with_builtin_adapters();
        for kind in [
            AgentAdapter::ClaudeCode,
            AgentAdapter::Codex,
            AgentAdapter::Pi,
            AgentAdapter::Omp,
            AgentAdapter::Custom,
        ] {
            assert!(
                reg.adapter_for(kind).is_some(),
                "missing adapter_for({kind:?})"
            );
        }
    }

    #[test]
    fn adapter_for_returns_none_for_unregistered_kind() {
        // Registry holds only Custom → other kinds lookup as None.
        let mut reg = AdapterRegistry::empty();
        reg.register(AgentAdapter::Custom, Arc::new(CustomCommandAdapter));
        assert!(reg.adapter_for(AgentAdapter::Custom).is_some());
        assert!(reg.adapter_for(AgentAdapter::Codex).is_none());
        assert!(reg.adapter_for(AgentAdapter::Pi).is_none());
        assert!(reg.adapter_for(AgentAdapter::ClaudeCode).is_none());
    }

    #[test]
    fn adapter_by_id_matches_each_builtin_slug() {
        let reg = AdapterRegistry::with_builtin_adapters();
        for slug in ["claude-code", "codex", "pi", "omp", "custom"] {
            let a = reg
                .adapter_by_id(slug)
                .unwrap_or_else(|| panic!("missing adapter_by_id({slug})"));
            assert_eq!(a.id(), slug, "id slug round-trips");
        }
    }

    #[test]
    fn adapter_by_id_returns_none_for_unknown_slug() {
        let reg = AdapterRegistry::with_builtin_adapters();
        assert!(reg.adapter_by_id("not-a-real-adapter").is_none());
        assert!(reg.adapter_by_id("").is_none());
    }

    #[test]
    fn register_appends_to_end() {
        // Behavioural assertion only — never touches `reg.slots` so a
        // future move to `IndexMap` or similar storage doesn't break the
        // test for structural reasons (L1 in review 260520-1639).
        let mut reg = AdapterRegistry::empty();
        reg.register(
            AgentAdapter::Custom,
            Arc::new(CountingMock::new("mock", false)),
        );
        assert_eq!(reg.len(), 1);
        reg.register(AgentAdapter::ClaudeCode, Arc::new(CustomCommandAdapter));
        assert_eq!(reg.len(), 2);
        // First-match-wins on duplicate kinds; lookup proves the mock
        // (registered first) is the one returned even after a second
        // adapter lands later in the vector.
        assert_eq!(
            reg.adapter_for(AgentAdapter::Custom).unwrap().id(),
            "mock",
            "first-match-wins on duplicate kinds"
        );
        // Second registration is reachable under its own kind.
        assert_eq!(
            reg.adapter_for(AgentAdapter::ClaudeCode).unwrap().id(),
            "custom",
        );
    }

    #[tokio::test]
    async fn detect_available_returns_one_entry_per_adapter() {
        let reg = AdapterRegistry::with_builtin_adapters();
        let entries = reg.detect_available().await;
        assert_eq!(entries.len(), 5);
        let kinds: Vec<AgentAdapter> = entries.iter().map(|e| e.adapter_enum).collect();
        assert_eq!(
            kinds,
            vec![
                AgentAdapter::ClaudeCode,
                AgentAdapter::Codex,
                AgentAdapter::Pi,
                AgentAdapter::Omp,
                AgentAdapter::Custom,
            ],
        );
    }

    #[tokio::test]
    async fn detect_available_marks_custom_as_available() {
        // Custom adapter always reports true — the universal escape hatch.
        // Any environment-dependent adapter (Claude Code / Codex / Pi)
        // would be flaky to assert here, so we lock the Custom row only.
        let reg = AdapterRegistry::with_builtin_adapters();
        let entries = reg.detect_available().await;
        let custom = entries
            .iter()
            .find(|e| e.adapter_enum == AgentAdapter::Custom)
            .expect("Custom entry present");
        assert!(custom.available, "Custom must always be available");
        assert_eq!(custom.adapter_id, "custom");
        assert_eq!(custom.display_name, "Custom command");
    }

    #[tokio::test]
    async fn detect_available_invokes_detect_exactly_once_per_adapter() {
        let mock = Arc::new(CountingMock::new("mock", false));
        let mut reg = AdapterRegistry::empty();
        reg.register(AgentAdapter::Custom, mock.clone());
        let entries = reg.detect_available().await;
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].available, "mock reports unavailable");
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn detect_available_folds_err_to_unavailable() {
        // Erroring adapter must not panic the registry walk; it should
        // surface as available=false alongside the surviving entry.
        // Use a second `CountingMock` (Ok(true)) rather than the real
        // CustomCommandAdapter for the "available next entry" assertion
        // so the test concern is purely "Err doesn't poison the walk"
        // and doesn't accidentally also assert Custom's detection
        // behaviour (L2 in review 260520-1639).
        let mut reg = AdapterRegistry::empty();
        reg.register(AgentAdapter::Custom, Arc::new(ErroringMock));
        reg.register(
            AgentAdapter::ClaudeCode,
            Arc::new(CountingMock::new("ok-mock", true)),
        );
        let entries = reg.detect_available().await;
        assert_eq!(entries.len(), 2);
        assert!(
            !entries[0].available,
            "Err must fold to available=false, not propagate"
        );
        assert!(
            entries[1].available,
            "second adapter unaffected by first's error"
        );
    }
}
