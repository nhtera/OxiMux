//! The headless [`SessionCatalog`]: persisted sessions enumerated straight
//! from storage, opened by resuming the agent — no window, no panes.
//!
//! The index of what exists is the `agent_chat:` blob set (the same one the
//! desktop writes), scanned once at boot and maintained by serve's own writes
//! thereafter — while serve runs it owns the store, since only one host can
//! hold the local socket at a time. That keeps `dormant()` at the
//! cheapest-possible cost its contract demands: a map read, not a re-parse of
//! every transcript per session-list snapshot.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::{ConnectSpec, connect};
use oximux_remote_host::catalog::{
    DormantChoice, DormantChoices, DormantSession, DormantTranscript, OpenGate, SessionCatalog,
};
use oximux_storage::SettingsRepo;

use super::blob::{self, CHAT_KEY_PREFIX, ChatBlob};
use super::pump::{self, PumpSet, PumpSpec};

/// What a list row needs, cached so `dormant()` never re-reads transcripts.
#[derive(Clone)]
struct IndexRow {
    title: Option<String>,
    model: Option<String>,
    cwd: Option<PathBuf>,
}

/// The shared session index: every session known to this host, live or not.
///
/// Shared (rather than owned by the catalog) because THREE parties write it —
/// the boot scan, the launcher when it mints a session, and each pump as its
/// fold evolves — and a session the index never learned would vanish from the
/// list the moment its agent died. Reads stay a map clone, which is what
/// keeps `dormant()` at its contract's cheapest-possible cost.
#[derive(Default)]
pub struct SessionIndex {
    rows: Mutex<HashMap<String, IndexRow>>,
}

impl SessionIndex {
    /// One session's recorded title — test-only observability.
    #[cfg(test)]
    pub fn title_of(&self, session_id: &str) -> Option<Option<String>> {
        self.rows.lock().unwrap().get(session_id).map(|r| r.title.clone())
    }

    /// Record (or refresh) one session's list-row facts.
    pub fn note(
        &self,
        session_id: &str,
        title: Option<String>,
        model: Option<String>,
        cwd: Option<PathBuf>,
    ) {
        self.rows
            .lock()
            .unwrap()
            .insert(session_id.to_string(), IndexRow { title, model, cwd });
    }
}

pub struct HeadlessCatalog {
    registry: Arc<SessionRegistry>,
    settings: SettingsRepo,
    pumps: Arc<PumpSet>,
    draining: Arc<AtomicBool>,
    gate: OpenGate,
    index: Arc<SessionIndex>,
    /// The local socket, for confining a resumed session's agent exactly as a
    /// freshly launched one is. A resume is the easy half of that job: the
    /// session id already exists, so the credential is granted under it
    /// directly and never needs rebinding.
    local: Arc<oximux_remote_local::LocalControlListener>,
}

impl HeadlessCatalog {
    /// Build the catalog, scanning the persisted blobs once. Blocking (a
    /// full-table read) — call during boot, before serving.
    pub fn scan(
        registry: Arc<SessionRegistry>,
        settings: SettingsRepo,
        pumps: Arc<PumpSet>,
        draining: Arc<AtomicBool>,
        index: Arc<SessionIndex>,
        local: Arc<oximux_remote_local::LocalControlListener>,
    ) -> Self {
        match settings.list_prefixed(CHAT_KEY_PREFIX) {
            Ok(rows) => {
                for (key, value) in rows {
                    let session_id = key.trim_start_matches(CHAT_KEY_PREFIX).to_string();
                    if session_id.is_empty() {
                        continue;
                    }
                    let Ok(blob) = serde_json::from_str::<ChatBlob>(&value) else {
                        tracing::warn!(session_id, "unreadable chat blob skipped from catalog");
                        continue;
                    };
                    index.note(
                        &session_id,
                        blob.derived_title(),
                        blob.model.clone(),
                        blob.session_meta.cwd.clone().map(PathBuf::from),
                    );
                }
            }
            Err(err) => tracing::warn!(%err, "chat blob scan failed; catalog starts empty"),
        }
        tracing::info!(sessions = index.rows.lock().unwrap().len(), "session catalog scanned");
        Self { registry, settings, pumps, draining, gate: OpenGate::new(), index, local }
    }

    /// The resume path: reconnect the persisted session's agent and pump it.
    async fn resume(&self, session_id: &str) -> Result<(), String> {
        if self.draining.load(Ordering::SeqCst) {
            return Err("the host is shutting down".into());
        }
        // Already live: idempotent success, per the trait contract.
        if self.registry.get(session_id).is_some() {
            return Ok(());
        }
        let blob = blob::load(&self.settings, session_id)
            .ok_or_else(|| "this host has no such session".to_string())?;
        let cwd = blob
            .session_meta
            .cwd
            .clone()
            .ok_or_else(|| "this session's working directory is unknown".to_string())?;
        let mut spec = ConnectSpec::for_backend(
            &oximux_agents::thread::ChatBackend {
                transport: blob.provider,
                acp_command: blob.acp_command.clone(),
                acp_args: blob.acp_args.clone(),
            },
            PathBuf::from(&cwd),
            // Resume on the model the session last ran, resolved: the picker
            // cache's current beats the launch-time pin.
            blob.choices.current_model.clone().or_else(|| blob.model.clone()),
            Some(session_id.to_string()),
            None,
            None,
        );
        spec.codex_posture = blob.codex_posture.clone();
        spec.pi_posture = blob.pi_posture.clone();
        // The resumed agent is confined to the session it is resuming. The id
        // is known here, so the credential is granted under it and is correctly
        // scoped from its very first use.
        let secret = self.local.grant_session(session_id);
        spec.env = super::launcher::credential_env(session_id, &secret);
        // `connect` spawns a process: off the async reactor.
        let spawned = tokio::task::spawn_blocking(move || connect(spec)).await;
        let (conn, events) = match spawned {
            Ok(Ok(pair)) => pair,
            Ok(Err(err)) => {
                self.local.revoke_session(session_id);
                tracing::warn!(%err, session_id, "headless resume spawn failed");
                return Err("the session could not be reopened".to_string());
            }
            Err(_) => {
                self.local.revoke_session(session_id);
                return Err("the resume task failed".to_string());
            }
        };
        let handle = self.registry.register(session_id.to_string(), conn);
        self.index.note(
            session_id,
            blob.derived_title(),
            blob.model.clone(),
            Some(PathBuf::from(&cwd)),
        );
        // A resumed backend replays nothing, so there is nothing to buffer:
        // the pump publishes the persisted fold immediately and the live
        // stream extends it from the next event on.
        pump::start(
            PumpSpec {
                session_id: session_id.to_string(),
                handle,
                events,
                buffered: Vec::new(),
                seed: blob,
                settings: self.settings.clone(),
                registry: self.registry.clone(),
                index: self.index.clone(),
                on_end: Some({
                    let local = self.local.clone();
                    let session_id = session_id.to_string();
                    Box::new(move || local.revoke_session(&session_id))
                }),
            },
            self.pumps.clone(),
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionCatalog for HeadlessCatalog {
    fn dormant(&self) -> Vec<DormantSession> {
        self.index
            .rows
            .lock()
            .unwrap()
            .iter()
            // The registry is authoritative for live sessions.
            .filter(|(id, _)| self.registry.get(id).is_none())
            .map(|(id, row)| DormantSession {
                session_id: id.clone(),
                title: row.title.clone(),
                model: row.model.clone(),
                cwd: row.cwd.clone(),
            })
            .collect()
    }

    fn transcript(&self, session_id: &str) -> Option<DormantTranscript> {
        let blob = blob::load(&self.settings, session_id)?;
        let entries_json = serde_json::to_string(&blob.entries).unwrap_or_else(|_| "[]".into());
        Some(DormantTranscript { entries_json, model: blob.model })
    }

    fn choices(&self, session_id: &str) -> Option<DormantChoices> {
        let blob = blob::load(&self.settings, session_id)?;
        let map = |c: &oximux_agents::thread::ModelChoice| DormantChoice {
            id: c.wire.clone(),
            label: c.label.clone(),
            description: c.description.clone(),
        };
        Some(DormantChoices {
            models: blob.choices.models.iter().map(map).collect(),
            modes: blob
                .choices
                .modes
                .iter()
                .map(|m| DormantChoice { id: m.wire.clone(), label: m.label.clone(), description: None })
                .collect(),
            current_model: blob.choices.current_model.clone(),
            current_mode: blob.choices.current_mode.clone(),
        })
    }

    async fn open(&self, session_id: &str) -> Result<(), String> {
        self.gate.open(session_id, || self.resume(session_id)).await
    }
}
