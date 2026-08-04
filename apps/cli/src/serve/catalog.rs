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

pub struct HeadlessCatalog {
    registry: Arc<SessionRegistry>,
    settings: SettingsRepo,
    pumps: Arc<PumpSet>,
    draining: Arc<AtomicBool>,
    gate: OpenGate,
    index: Mutex<HashMap<String, IndexRow>>,
}

impl HeadlessCatalog {
    /// Build the catalog, scanning the persisted blobs once. Blocking (a
    /// full-table read) — call during boot, before serving.
    pub fn scan(
        registry: Arc<SessionRegistry>,
        settings: SettingsRepo,
        pumps: Arc<PumpSet>,
        draining: Arc<AtomicBool>,
    ) -> Self {
        let mut index = HashMap::new();
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
                    index.insert(
                        session_id,
                        IndexRow {
                            title: blob.derived_title(),
                            model: blob.model.clone(),
                            cwd: blob.session_meta.cwd.clone().map(PathBuf::from),
                        },
                    );
                }
            }
            Err(err) => tracing::warn!(%err, "chat blob scan failed; catalog starts empty"),
        }
        tracing::info!(sessions = index.len(), "session catalog scanned");
        Self {
            registry,
            settings,
            pumps,
            draining,
            gate: OpenGate::new(),
            index: Mutex::new(index),
        }
    }

    /// Serve's own writes keep the index current (the launcher calls this for
    /// new sessions; resumes refresh their row on open).
    pub fn note_session(&self, session_id: &str, title: Option<String>, model: Option<String>, cwd: Option<PathBuf>) {
        self.index
            .lock()
            .unwrap()
            .insert(session_id.to_string(), IndexRow { title, model, cwd });
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
        // `connect` spawns a process: off the async reactor.
        let (conn, events) = tokio::task::spawn_blocking(move || connect(spec))
            .await
            .map_err(|_| "the resume task failed".to_string())?
            .map_err(|err| {
                tracing::warn!(%err, session_id, "headless resume spawn failed");
                "the session could not be reopened".to_string()
            })?;
        let handle = self.registry.register(session_id.to_string(), conn);
        self.note_session(
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
