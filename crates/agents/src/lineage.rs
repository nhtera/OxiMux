//! Spawn lineage: which agent started which, under which caps.
//!
//! An agent may start child agents. The host has to remember the tree to cap
//! it (depth, fan-out, open children, a lifetime budget), to let a parent
//! reach only its own subtree, and later to route a child's events back to its
//! parent. All of that must survive a restart, so it is a table, not a map.
//!
//! **One choke point.** Every agent-origin or run-origin child is created
//! through [`LineageStore::reserve`], which takes the creator as a required
//! argument and computes the child's depth itself, so no caller can understate
//! it. A path that does not reserve cannot create a lineage row.
//!
//! **Reserve, launch, finalize.** A launch awaits for seconds, and two
//! dispatchers share one host. So the slot is taken in one short
//! `BEGIN IMMEDIATE` transaction (reconcile, idempotency, caps, insert
//! `reserved`), committed *before* the launch, and settled by a second one
//! afterwards ([`LineageStore::finalize`] or [`LineageStore::rollback`]).
//! Never one transaction across the launch: the connection is shared.
//!
//! **States.** `reserved → active ⇄ parked → ended`. Caps count only children
//! that are working (`reserved|active`); a finished child parks and frees its
//! slot, and reviving it needs none. `ended` is explicit only (cancel, cascade,
//! a run's end): quitting the app, closing a tab or a process exit never ends
//! a row.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::thread::launch_posture::SpawnPosture;

/// The parent id of a root the operator started (no agent parent).
pub const USER_ROOT: &str = "-";

/// The hard ceiling on spawn depth, whatever the setting says.
pub const MAX_DEPTH: u32 = 2;

/// A `reserved` row older than this is a launch that never finalised (the host
/// crashed mid-launch, or the launch hung) and is reclaimed. Generous on
/// purpose: a cold CLI, MCP start-up or a first-run install can take well over
/// a minute, and reclaiming a launch that is merely slow lets a replay start a
/// second child (the slow one's `finalize` then fails and it is stopped).
pub const RESERVATION_TIMEOUT_S: i64 = 300;

/// How long a freshly active row is counted as working whatever its session
/// reports: a child just launched has not said it is working yet.
pub const ACTIVE_GRACE_S: i64 = 30;

/// Consecutive failed launches after which a parent's spawns are refused.
pub const START_FAILURE_LIMIT: u32 = 3;

/// How long a tripped breaker refuses before letting one probe through.
pub const START_FAILURE_COOLDOWN_S: i64 = 60;

/// How many parked ids an open-cap refusal lists.
const PARKED_IDS_SHOWN: usize = 8;

/// Where a row stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineageState {
    /// A slot taken for a launch in flight.
    Reserved,
    /// Working: running, blocked on a prompt, rate-limited, or with a queue.
    Active,
    /// Idle with nothing pending, or failed and not yet cancelled.
    Parked,
    /// Cancelled, cascaded, or its run ended. Terminal.
    Ended,
}

impl LineageState {
    pub fn as_text(self) -> &'static str {
        match self {
            LineageState::Reserved => "reserved",
            LineageState::Active => "active",
            LineageState::Parked => "parked",
            LineageState::Ended => "ended",
        }
    }

    /// An unrecognised stored value reads as `Parked`: it holds no working
    /// slot (so it cannot wedge a cap) and is not terminal (so nothing is lost).
    fn from_text(text: &str) -> Self {
        match text {
            "reserved" => LineageState::Reserved,
            "active" => LineageState::Active,
            "ended" => LineageState::Ended,
            _ => LineageState::Parked,
        }
    }
}

/// Who asked for a spawn, derived by the host from the caller's credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOrigin {
    /// The operator (Full scope).
    User,
    /// An agent, through its session credential.
    Agent,
    /// A team or plan runner, for one of its roles.
    Team,
}

impl SpawnOrigin {
    pub fn as_text(self) -> &'static str {
        match self {
            SpawnOrigin::User => "user",
            SpawnOrigin::Agent => "agent",
            SpawnOrigin::Team => "team",
        }
    }

    /// Unknown reads as `Agent`, the most capped origin.
    fn from_text(text: &str) -> Self {
        match text {
            "user" => SpawnOrigin::User,
            "team" => SpawnOrigin::Team,
            _ => SpawnOrigin::Agent,
        }
    }
}

/// Which of a child's events reach its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifyParent {
    /// Every event.
    #[default]
    Auto,
    /// Only failed, interrupted, needs permission, needs input.
    Stuck,
    /// None (the rail and the inbox still show the child).
    Off,
}

impl NotifyParent {
    pub fn as_text(self) -> &'static str {
        match self {
            NotifyParent::Auto => "auto",
            NotifyParent::Stuck => "stuck",
            NotifyParent::Off => "off",
        }
    }

    fn from_text(text: &str) -> Self {
        match text {
            "stuck" => NotifyParent::Stuck,
            "off" => NotifyParent::Off,
            _ => NotifyParent::Auto,
        }
    }
}

/// The parent a spawn hangs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnParent {
    /// A new root the operator starts: no parent, depth 0.
    UserRoot,
    /// A child of this session.
    Session(String),
    /// A role of a team or plan run: the synthetic parent `team:<run>` or
    /// `plan:<run>`. Its roles are depth 1.
    Run(String),
}

impl SpawnParent {
    fn parent_id(&self) -> &str {
        match self {
            SpawnParent::UserRoot => USER_ROOT,
            SpawnParent::Session(id) | SpawnParent::Run(id) => id,
        }
    }
}

/// Spawn limits, read from host settings on every reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnCaps {
    /// Deepest child allowed (clamped to [`MAX_DEPTH`]).
    pub depth: u32,
    /// Working children one parent may have (agent origin).
    pub active_per_parent: u32,
    /// Working children one root's tree may have (agent origin).
    pub active_per_root: u32,
    /// Children one root's tree may hold open, parked included (agent origin).
    pub open_per_root: u32,
    /// Children one root's tree may ever spawn (agent origin). Not settable.
    pub lifetime_per_root: u32,
    /// Open roles one team or plan run may hold.
    pub run_lane: u32,
}

impl Default for SpawnCaps {
    fn default() -> Self {
        Self {
            depth: 1,
            active_per_parent: 8,
            active_per_root: 8,
            open_per_root: 16,
            lifetime_per_root: 100,
            run_lane: 16,
        }
    }
}

impl SpawnCaps {
    /// The caps with the depth clamped to the hard ceiling. A stored or remote
    /// value above it never takes effect.
    pub fn clamped(self) -> Self {
        Self { depth: self.depth.min(MAX_DEPTH), ..self }
    }
}

/// What the child is asked to be. Everything here is stored on its row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// The caller's idempotency key, unique per parent.
    pub request_id: String,
    pub origin: SpawnOrigin,
    pub posture: SpawnPosture,
    pub notify_parent: NotifyParent,
    pub allow_parent_permits: bool,
    /// The child's id when the provider lets the host pick it (Claude).
    pub preassigned_id: Option<String>,
    /// Whatever else identifies the request (agent, model, prompt, cwd), so a
    /// replayed key can be told from a reused one. Only its digest is stored.
    pub fingerprint_input: String,
}

/// Why a reservation was refused. Each agent-origin refusal's text ends with a
/// calm instruction rather than a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnRefusal {
    Depth { depth: u32, max: u32 },
    Fanout { active: u32, limit: u32 },
    Open { open: u32, limit: u32, parked_ids: Vec<String> },
    Budget { spawned: u32, limit: u32 },
    StartFailing { count: u32, last_reason: String },
    /// The key was used before for a different spawn.
    IdempotencyConflict,
    /// The same spawn is still launching.
    InProgress,
    /// The parent itself was cancelled.
    ParentEnded,
}

impl std::fmt::Display for SpawnRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnRefusal::Depth { depth, max } => write!(
                f,
                "Spawn refused: depth {depth} is past the limit of {max}. Spawning is not \
                 available at this depth. Complete this task yourself."
            ),
            SpawnRefusal::Fanout { active, limit } => write!(
                f,
                "Spawn refused: {active} of {limit} active children. Wait on a child, or \
                 cancel one."
            ),
            SpawnRefusal::Open { open, limit, parked_ids } => write!(
                f,
                "Spawn refused: {open} of {limit} open children (finished ones included: \
                 {}). Cancel finished children you no longer need.",
                parked_ids.join(", ")
            ),
            SpawnRefusal::Budget { spawned, limit } => write!(
                f,
                "Spawn refused: this task has spawned {spawned} of {limit} agents. Complete \
                 this task yourself."
            ),
            SpawnRefusal::StartFailing { count, last_reason } => write!(
                f,
                "Spawn refused: the last {count} launches failed ({last_reason}). Launching \
                 is failing; fix the cause or complete the task yourself."
            ),
            SpawnRefusal::IdempotencyConflict => write!(
                f,
                "Spawn refused: this request id was already used for a different spawn. Use \
                 a new request id."
            ),
            SpawnRefusal::InProgress => write!(
                f,
                "Spawn refused: the same request is still launching. Wait for it to finish."
            ),
            SpawnRefusal::ParentEnded => write!(
                f,
                "Spawn refused: this agent was cancelled. Stop and report what you have."
            ),
        }
    }
}

/// One lineage row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageRow {
    pub row_id: i64,
    /// `None` only while reserved for a provider that names itself later.
    pub session_id: Option<String>,
    pub parent_id: String,
    pub root_id: String,
    pub depth: u32,
    pub request_id: String,
    pub origin: SpawnOrigin,
    pub posture: SpawnPosture,
    pub state: LineageState,
    pub allow_parent_permits: bool,
    pub notify_parent: NotifyParent,
    pub identity_slot: Option<u32>,
    pub created_at: DateTime<Local>,
    pub ended_at: Option<DateTime<Local>>,
}

/// A slot taken by [`LineageStore::reserve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reservation {
    /// A new slot: launch the child, then finalize or roll back this row.
    Fresh { row_id: i64, depth: u32, root_id: String },
    /// The same request was already spawned: answer with this row, launch
    /// nothing.
    Replayed(LineageRow),
}

/// Asks the host which sessions are working right now (running, blocked on a
/// prompt, rate-limited, or with a non-empty queue). The store reconciles
/// `active` rows against it, so a crash can never leave a slot held forever.
pub trait WorkingSessions {
    fn is_working(&self, session_id: &str) -> bool;
}

impl<F: Fn(&str) -> bool> WorkingSessions for F {
    fn is_working(&self, session_id: &str) -> bool {
        self(session_id)
    }
}

/// The lineage table, backed by the app's SQLite database. Cloning shares the
/// connection.
#[derive(Clone)]
pub struct LineageStore {
    conn: Arc<Mutex<Connection>>,
}

const COLUMNS: &str = "id, session_id, parent_id, root_id, depth, request_id, origin, posture, \
                       state, allow_parent_permits, notify_parent, identity_slot, created_at, \
                       ended_at";

impl LineageStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// The child-event outbox in the same database (the two tables move
    /// together on a rekey).
    pub fn outbox(&self) -> crate::outbox::Outbox {
        crate::outbox::Outbox::new(self.conn.clone())
    }

    /// Take a slot for a child of `parent`, or refuse.
    ///
    /// In one `BEGIN IMMEDIATE` transaction: reconcile the parent's tree
    /// (stale reservations deleted, idle `active` rows parked), answer a
    /// replayed request id, check the failed-start breaker and the caps, then
    /// insert a `reserved` row. Commit before launching; settle with
    /// [`Self::finalize`] or [`Self::rollback`].
    pub fn reserve(
        &self,
        parent: &SpawnParent,
        spec: &SpawnSpec,
        caps: SpawnCaps,
        working: &dyn WorkingSessions,
        now: DateTime<Local>,
    ) -> Result<std::result::Result<Reservation, SpawnRefusal>> {
        let caps = caps.clamped();
        let parent_id = parent.parent_id();
        let fingerprint = fingerprint(parent_id, spec);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin lineage reservation")?;

        // Where the child sits: its depth and the root whose budget it draws on.
        let (depth, root_id) = match parent {
            SpawnParent::UserRoot => (0, String::new()),
            SpawnParent::Run(run) => (1, run.clone()),
            SpawnParent::Session(id) => match row_by_session(&tx, id)? {
                // A cancelled agent that is still running spawns nothing more.
                Some(row) if row.state == LineageState::Ended => {
                    return Ok(Err(SpawnRefusal::ParentEnded));
                }
                Some(row) => (row.depth + 1, row.root_id),
                // A session no row names as a child is a root.
                None => (1, id.clone()),
            },
        };

        // A replay first, before any sweep can touch its row: a launch still
        // in flight answers `InProgress`; only one past the timeout is
        // reclaimed (and then reserved afresh below).
        if let Some(existing) = row_by_request(&tx, parent_id, &spec.request_id)? {
            let (stored, updated_at): (String, String) = tx.query_row(
                "SELECT request_fingerprint, updated_at FROM agent_lineage WHERE id = ?1",
                params![existing.row_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if stored != fingerprint {
                return Ok(Err(SpawnRefusal::IdempotencyConflict));
            }
            if existing.state != LineageState::Reserved {
                return Ok(Ok(Reservation::Replayed(existing)));
            }
            if parse_time(&updated_at) + chrono::Duration::seconds(RESERVATION_TIMEOUT_S) > now {
                return Ok(Err(SpawnRefusal::InProgress));
            }
            tx.execute("DELETE FROM agent_lineage WHERE id = ?1", params![existing.row_id])?;
        }

        reconcile_tree(&tx, &root_id, working, now)?;

        if depth > caps.depth {
            return Ok(Err(SpawnRefusal::Depth { depth, max: caps.depth }));
        }

        if matches!(spec.origin, SpawnOrigin::Agent | SpawnOrigin::Team)
            && let Some(refusal) = breaker(&tx, parent_id, now)?
        {
            return Ok(Err(refusal));
        }

        match spec.origin {
            SpawnOrigin::Agent => {
                let active_parent = count(
                    &tx,
                    "parent_id = ?1 AND origin = 'agent' AND state IN ('reserved','active')",
                    parent_id,
                )?;
                if active_parent >= caps.active_per_parent {
                    return Ok(Err(SpawnRefusal::Fanout {
                        active: active_parent,
                        limit: caps.active_per_parent,
                    }));
                }
                let active_root = count(
                    &tx,
                    "root_id = ?1 AND origin = 'agent' AND state IN ('reserved','active')",
                    &root_id,
                )?;
                if active_root >= caps.active_per_root {
                    return Ok(Err(SpawnRefusal::Fanout {
                        active: active_root,
                        limit: caps.active_per_root,
                    }));
                }
                let open = count(
                    &tx,
                    "root_id = ?1 AND origin = 'agent' AND state != 'ended'",
                    &root_id,
                )?;
                if open >= caps.open_per_root {
                    return Ok(Err(SpawnRefusal::Open {
                        open,
                        limit: caps.open_per_root,
                        parked_ids: parked_ids(&tx, &root_id)?,
                    }));
                }
                let spawned = count(&tx, "root_id = ?1 AND origin = 'agent'", &root_id)?;
                if spawned >= caps.lifetime_per_root {
                    return Ok(Err(SpawnRefusal::Budget {
                        spawned,
                        limit: caps.lifetime_per_root,
                    }));
                }
            }
            SpawnOrigin::Team => {
                let open = count(&tx, "parent_id = ?1 AND state != 'ended'", parent_id)?;
                if open >= caps.run_lane {
                    return Ok(Err(SpawnRefusal::Open {
                        open,
                        limit: caps.run_lane,
                        parked_ids: Vec::new(),
                    }));
                }
            }
            // The operator is capped by depth only.
            SpawnOrigin::User => {}
        }

        let stamp = stamp(now);
        // A user root is its own root; with no id yet, `finalize` sets it.
        let stored_root = match parent {
            SpawnParent::UserRoot => spec.preassigned_id.clone().unwrap_or_default(),
            _ => root_id.clone(),
        };
        tx.execute(
            "INSERT INTO agent_lineage (session_id, parent_id, root_id, depth, request_id, \
             request_fingerprint, origin, posture, state, allow_parent_permits, notify_parent, \
             created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'reserved', ?9, ?10, ?11, ?11)",
            params![
                spec.preassigned_id,
                parent_id,
                stored_root,
                depth,
                spec.request_id,
                fingerprint,
                spec.origin.as_text(),
                spec.posture.as_text(),
                spec.allow_parent_permits,
                spec.notify_parent.as_text(),
                stamp,
            ],
        )
        .context("insert lineage reservation")?;
        let row_id = tx.last_insert_rowid();
        tx.commit().context("commit lineage reservation")?;
        Ok(Ok(Reservation::Fresh { row_id, depth, root_id: stored_root }))
    }

    /// The launch succeeded: the row becomes `active` under `session_id`, and
    /// the parent's failed-start counter resets.
    pub fn finalize(&self, row_id: i64, session_id: &str, now: DateTime<Local>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin lineage finalize")?;
        let parent: String = tx
            .query_row(
                "SELECT parent_id FROM agent_lineage WHERE id = ?1 AND state = 'reserved'",
                params![row_id],
                |r| r.get(0),
            )
            .context("finalize: the reservation is gone (reclaimed or its parent ended)")?;
        let changed = tx
            .execute(
                "UPDATE agent_lineage SET session_id = ?2, state = 'active', updated_at = ?3, \
                 root_id = CASE WHEN parent_id = ?4 THEN ?2 ELSE root_id END \
                 WHERE id = ?1 AND state = 'reserved'",
                params![row_id, session_id, stamp(now), USER_ROOT],
            )
            .context("finalize lineage row")?;
        anyhow::ensure!(changed == 1, "finalize: the reservation is gone");
        tx.execute("DELETE FROM agent_spawn_failures WHERE parent_id = ?1", params![parent])?;
        tx.commit().context("commit lineage finalize")?;
        Ok(())
    }

    /// The launch failed: release the slot and count the failure against the
    /// parent, so retrying with a fresh request id cannot launder it.
    pub fn rollback(&self, row_id: i64, reason: &str, now: DateTime<Local>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin lineage rollback")?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT parent_id, origin FROM agent_lineage WHERE id = ?1 AND state = 'reserved'",
                params![row_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        tx.execute("DELETE FROM agent_lineage WHERE id = ?1 AND state = 'reserved'", params![row_id])?;
        if let Some((parent, origin)) = row
            && origin != SpawnOrigin::User.as_text()
        {
            tx.execute(
                "INSERT INTO agent_spawn_failures (parent_id, count, last_reason, last_at) \
                 VALUES (?1, 1, ?2, ?3) ON CONFLICT(parent_id) DO UPDATE SET \
                 count = count + 1, last_reason = excluded.last_reason, last_at = excluded.last_at",
                params![parent, reason, stamp(now)],
            )?;
        }
        tx.commit().context("commit lineage rollback")?;
        Ok(())
    }

    /// A provider renamed its session (the desktop's placeholder id, or a
    /// self-naming provider's first turn). Every reference follows: the row,
    /// its children, its tree, the outbox, the breaker.
    pub fn rekey(&self, old: &str, new: &str) -> Result<()> {
        if old == new {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin lineage rekey")?;
        tx.execute("UPDATE agent_lineage SET session_id = ?2 WHERE session_id = ?1", params![old, new])?;
        tx.execute("UPDATE agent_lineage SET parent_id = ?2 WHERE parent_id = ?1", params![old, new])?;
        tx.execute("UPDATE agent_lineage SET root_id = ?2 WHERE root_id = ?1", params![old, new])?;
        tx.execute(
            "UPDATE OR REPLACE agent_event_outbox SET parent_session = ?2 WHERE parent_session = ?1",
            params![old, new],
        )?;
        // The payload names the child too (the envelope shows it). `OR REPLACE`:
        // the same edge recorded under both ids is one event.
        tx.execute(
            "UPDATE OR REPLACE agent_event_outbox SET child = ?2, payload = json_set(payload, '$.child', ?2) \
             WHERE child = ?1",
            params![old, new],
        )?;
        tx.execute(
            "UPDATE OR REPLACE agent_spawn_failures SET parent_id = ?2 WHERE parent_id = ?1",
            params![old, new],
        )?;
        tx.commit().context("commit lineage rekey")?;
        Ok(())
    }

    /// A parked child started a new turn. Needs no slot and no cap check.
    pub fn activate(&self, session_id: &str, now: DateTime<Local>) -> Result<bool> {
        self.set_state(session_id, "parked", LineageState::Active, now)
    }

    /// A working child went idle with nothing pending (or failed). A row made
    /// active in the last [`ACTIVE_GRACE_S`] stays active: a child that has
    /// just started has not reported working yet, and parking it would let a
    /// burst of spawns slip past the active caps.
    pub fn park(&self, session_id: &str, now: DateTime<Local>) -> Result<bool> {
        let settled = stamp(now - chrono::Duration::seconds(ACTIVE_GRACE_S));
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE agent_lineage SET state = 'parked', updated_at = ?3 \
             WHERE session_id = ?1 AND state = 'active' AND updated_at < ?2",
            params![session_id, settled, stamp(now)],
        )?;
        Ok(n > 0)
    }

    fn set_state(
        &self,
        session_id: &str,
        from: &str,
        to: LineageState,
        now: DateTime<Local>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE agent_lineage SET state = ?3, updated_at = ?4 \
             WHERE session_id = ?1 AND state = ?2",
            params![session_id, from, to.as_text(), stamp(now)],
        )?;
        Ok(n > 0)
    }

    /// End `id` (a session, or a synthetic `team:<run>` / `plan:<run>`), and
    /// with `cascade` every non-ended row beneath it. Answered from the table:
    /// a dormant child is ended without being opened. Returns the session ids
    /// ended, deepest first, for the caller to stop.
    pub fn mark_ended(&self, id: &str, cascade: bool, now: DateTime<Local>) -> Result<Vec<String>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin lineage end")?;
        let mut ended = Vec::new();
        let mut frontier = vec![id.to_string()];
        if cascade {
            // Walk through rows already ended too: a child cancelled on its
            // own may still have working descendants. Only the UPDATE below
            // skips ended rows.
            let mut seen: std::collections::HashSet<String> = frontier.iter().cloned().collect();
            let mut i = 0;
            while i < frontier.len() {
                let parent = frontier[i].clone();
                let kids: Vec<String> = tx
                    .prepare(
                        "SELECT session_id FROM agent_lineage \
                         WHERE parent_id = ?1 AND session_id IS NOT NULL",
                    )?
                    .query_map(params![parent], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                frontier.extend(kids.into_iter().filter(|k| seen.insert(k.clone())));
                i += 1;
            }
        }
        let stamp = stamp(now);
        for sid in frontier.iter().rev() {
            let n = tx.execute(
                "UPDATE agent_lineage SET state = 'ended', ended_at = ?2, updated_at = ?2 \
                 WHERE session_id = ?1 AND state != 'ended'",
                params![sid, stamp],
            )?;
            if n > 0 {
                ended.push(sid.clone());
            }
        }
        // Reservations under an ended parent never launch.
        for parent in &frontier {
            tx.execute(
                "DELETE FROM agent_lineage WHERE parent_id = ?1 AND state = 'reserved'",
                params![parent],
            )?;
        }
        tx.commit().context("commit lineage end")?;
        Ok(ended)
    }

    /// End every role of a run and everything beneath them.
    pub fn end_run(&self, run_parent: &str, now: DateTime<Local>) -> Result<Vec<String>> {
        self.mark_ended(run_parent, true, now)
    }

    /// Host start: the previous owner of this data dir is gone (the host lock
    /// proves it), so nothing it reserved will launch and nothing it ran is
    /// still working. `reserved` rows are deleted; `active` rows park.
    pub fn boot_reconcile(&self, now: DateTime<Local>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM agent_lineage WHERE state = 'reserved'", [])?;
        conn.execute(
            "UPDATE agent_lineage SET state = 'parked', updated_at = ?1 WHERE state = 'active'",
            params![stamp(now)],
        )?;
        Ok(())
    }

    /// Spawned sessions that are `active` or `parked`: the rows a host's status
    /// subscriber moves between the two as their sessions work and go idle.
    pub fn tracked(&self) -> Result<Vec<(String, LineageState)>> {
        let conn = self.conn.lock().unwrap();
        let rows = conn
            .prepare(
                "SELECT session_id, state FROM agent_lineage \
                 WHERE state IN ('active','parked') AND session_id IS NOT NULL",
            )?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(s, st)| (s, LineageState::from_text(&st))).collect())
    }

    /// The row for a session, if it was ever spawned through the grant.
    pub fn row(&self, session_id: &str) -> Result<Option<LineageRow>> {
        row_by_session(&self.conn.lock().unwrap(), session_id)
    }

    /// The session id a row holds now: a provider that names its own session
    /// is rekeyed onto that name after launch, under the same row.
    pub fn session_of_row(&self, row_id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT session_id FROM agent_lineage WHERE id = ?1", params![row_id], |r| r.get(0))
            .optional()?
            .flatten())
    }

    /// The row a request id produced under a parent.
    pub fn find_by_request(&self, parent_id: &str, request_id: &str) -> Result<Option<LineageRow>> {
        row_by_request(&self.conn.lock().unwrap(), parent_id, request_id)
    }

    /// Children of `of` (a session or a synthetic run parent), oldest first;
    /// with `recursive`, the whole subtree breadth-first. Ended rows included,
    /// reservations not.
    pub fn children(&self, of: &str, recursive: bool) -> Result<Vec<LineageRow>> {
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        let mut frontier = vec![of.to_string()];
        let mut i = 0;
        while i < frontier.len() {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM agent_lineage \
                 WHERE parent_id = ?1 AND state != 'reserved' ORDER BY id"
            ))?;
            let kids = stmt
                .query_map(params![frontier[i]], read_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for kid in kids {
                if recursive && let Some(sid) = &kid.session_id {
                    frontier.push(sid.clone());
                }
                out.push(kid);
            }
            if !recursive {
                break;
            }
            i += 1;
        }
        Ok(out)
    }

    /// Whether `target` sits strictly beneath `ancestor`.
    pub fn is_descendant(&self, ancestor: &str, target: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut current = target.to_string();
        // Depth is capped at MAX_DEPTH, so a short walk; the bound guards a
        // corrupted (cyclic) table.
        for _ in 0..=(MAX_DEPTH as usize + 1) {
            let parent: Option<String> = conn
                .query_row(
                    "SELECT parent_id FROM agent_lineage WHERE session_id = ?1",
                    params![current],
                    |r| r.get(0),
                )
                .optional()?;
            match parent {
                Some(p) if p == ancestor => return Ok(true),
                Some(p) if p != USER_ROOT => current = p,
                _ => return Ok(false),
            }
        }
        Ok(false)
    }

    /// A session's depth: its row's, or 0 for a session no row names (a root).
    pub fn depth_of(&self, session_id: &str) -> Result<u32> {
        Ok(self.row(session_id)?.map_or(0, |r| r.depth))
    }
}

/// Reconcile one tree inside a reservation: stale reservations go, and an
/// `active` row whose session is not working parks, so a crash can never hold
/// a slot forever.
fn reconcile_tree(
    conn: &Connection,
    root_id: &str,
    working: &dyn WorkingSessions,
    now: DateTime<Local>,
) -> Result<()> {
    let cutoff = stamp(now - chrono::Duration::seconds(RESERVATION_TIMEOUT_S));
    conn.execute(
        "DELETE FROM agent_lineage WHERE state = 'reserved' AND updated_at < ?1",
        params![cutoff],
    )?;
    if root_id.is_empty() {
        return Ok(());
    }
    let settled = stamp(now - chrono::Duration::seconds(ACTIVE_GRACE_S));
    let active: Vec<String> = conn
        .prepare(
            "SELECT session_id FROM agent_lineage WHERE root_id = ?1 AND state = 'active' \
             AND session_id IS NOT NULL AND updated_at < ?2",
        )?
        .query_map(params![root_id, settled], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let stamp = stamp(now);
    for sid in active.iter().filter(|s| !working.is_working(s)) {
        conn.execute(
            "UPDATE agent_lineage SET state = 'parked', updated_at = ?2 \
             WHERE session_id = ?1 AND state = 'active'",
            params![sid, stamp],
        )?;
    }
    Ok(())
}

/// The failed-start breaker: refuse while a parent's last
/// [`START_FAILURE_LIMIT`] launches failed within the cooldown. After the
/// cooldown one probe goes through; its success clears the counter.
fn breaker(conn: &Connection, parent_id: &str, now: DateTime<Local>) -> Result<Option<SpawnRefusal>> {
    let row: Option<(u32, String, String)> = conn
        .query_row(
            "SELECT count, last_reason, last_at FROM agent_spawn_failures WHERE parent_id = ?1",
            params![parent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((count, last_reason, last_at)) = row else { return Ok(None) };
    let cooling = parse_time(&last_at) + chrono::Duration::seconds(START_FAILURE_COOLDOWN_S) > now;
    Ok((count >= START_FAILURE_LIMIT && cooling)
        .then_some(SpawnRefusal::StartFailing { count, last_reason }))
}

/// `filter` is always a literal from this file (never caller data), which is
/// what makes interpolating it safe.
fn count(conn: &Connection, filter: &'static str, arg: &str) -> Result<u32> {
    conn.query_row(&format!("SELECT COUNT(*) FROM agent_lineage WHERE {filter}"), params![arg], |r| {
        r.get(0)
    })
    .context("count lineage rows")
}

fn parked_ids(conn: &Connection, root_id: &str) -> Result<Vec<String>> {
    conn.prepare(
        "SELECT session_id FROM agent_lineage WHERE root_id = ?1 AND origin = 'agent' \
         AND state = 'parked' AND session_id IS NOT NULL ORDER BY updated_at LIMIT ?2",
    )?
    .query_map(params![root_id, PARKED_IDS_SHOWN as i64], |r| r.get(0))?
    .collect::<rusqlite::Result<_>>()
    .context("read parked children")
}

fn row_by_session(conn: &Connection, session_id: &str) -> Result<Option<LineageRow>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM agent_lineage WHERE session_id = ?1"),
        params![session_id],
        read_row,
    )
    .optional()
    .context("read lineage row")
}

fn row_by_request(conn: &Connection, parent_id: &str, request_id: &str) -> Result<Option<LineageRow>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM agent_lineage WHERE parent_id = ?1 AND request_id = ?2"),
        params![parent_id, request_id],
        read_row,
    )
    .optional()
    .context("read lineage row by request")
}

fn read_row(r: &Row<'_>) -> rusqlite::Result<LineageRow> {
    Ok(LineageRow {
        row_id: r.get(0)?,
        session_id: r.get(1)?,
        parent_id: r.get(2)?,
        root_id: r.get(3)?,
        depth: r.get(4)?,
        request_id: r.get(5)?,
        origin: SpawnOrigin::from_text(&r.get::<_, String>(6)?),
        posture: SpawnPosture::from_text(&r.get::<_, String>(7)?),
        state: LineageState::from_text(&r.get::<_, String>(8)?),
        allow_parent_permits: r.get(9)?,
        notify_parent: NotifyParent::from_text(&r.get::<_, String>(10)?),
        identity_slot: r.get(11)?,
        created_at: parse_time(&r.get::<_, String>(12)?),
        ended_at: r.get::<_, Option<String>>(13)?.as_deref().map(parse_time),
    })
}

/// The stored digest of a request: everything that makes two spawns differ.
fn fingerprint(parent_id: &str, spec: &SpawnSpec) -> String {
    let mut h = Sha256::new();
    for part in [
        parent_id,
        spec.origin.as_text(),
        spec.posture.as_text(),
        spec.notify_parent.as_text(),
        if spec.allow_parent_permits { "1" } else { "0" },
        &spec.fingerprint_input,
    ] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    format!("{:x}", h.finalize())
}

/// Timestamps are stored in UTC so that comparing them as text (the stale
/// sweep does, in SQL) orders them correctly across a DST change or a moved
/// time zone.
fn stamp(at: DateTime<Local>) -> String {
    at.with_timezone(&chrono::Utc).to_rfc3339()
}

/// Unparseable timestamps read as the epoch: old enough to be reclaimed, never
/// new enough to hold a breaker shut.
fn parse_time(text: &str) -> DateTime<Local> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Local))
        .unwrap_or_else(|_| DateTime::<Local>::from(std::time::UNIX_EPOCH))
}

#[cfg(test)]
#[path = "lineage_tests.rs"]
mod tests;
