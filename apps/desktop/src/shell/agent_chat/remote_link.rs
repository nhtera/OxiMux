//! The chat view's link into the host's session registry: binding the live
//! connection under the session's remote id, re-keying it onto the agent's own
//! id once one is minted, and publishing what a remote client shows for it
//! (title, effective model and permission mode, transcript).
//!
//! Moved verbatim out of `agent_chat/mod.rs` (no behaviour change) to give the
//! view file headroom under the file-size ratchet; the orchestration work
//! (spawn credentials, lineage rekey, the working flag) lands here.

use gpui::{App, Context};
use oximux_agents::session_registry::SessionMeta;
use oximux_agents::thread::{ConnectSpec, McpServerSpec, Transport};

use super::{AgentChatView, RemoteControl};

impl AgentChatView {
    /// The id a Claude respawn should start under: the one chosen up front
    /// (`assemble`), while no prompt has reached it. Claude records a session
    /// on its first prompt and refuses the same `--session-id` after that; a
    /// session never prompted leaves nothing behind, so reusing the id keeps the
    /// registry, lineage and credential on it across an early respawn (a model
    /// switch, local access turned on, Retry). A placeholder (`agent-…`, a
    /// draft) is not a Claude id and is never passed.
    pub(super) fn unused_preassigned_id(&self) -> Option<String> {
        let unused = self.backend.transport == oximux_agents::thread::Transport::StreamJson
            && self.thread.session_id.is_none()
            && !self.thread.entries.iter().any(|e| matches!(e, super::ThreadEntry::User { .. }))
            && uuid::Uuid::parse_str(&self.remote_session_id).is_ok();
        unused.then(|| self.remote_session_id.clone())
    }

    /// Move this session onto the agent's own id once the agent has minted one.
    ///
    /// A chat that has never run is registered under a positional placeholder,
    /// which is worthless to a remote client the moment the desktop restarts.
    /// The agent's id names the conversation instead, so the session is re-keyed
    /// onto it at the first opportunity — which is also the first moment it has
    /// any history worth reaching from another device.
    ///
    /// Re-registering under the new id restarts `seq` at 1, so the old id is
    /// dropped only after the new one is live: a client subscribed to the
    /// placeholder sees its stream end and resubscribes, rather than sitting on a
    /// cursor no future event will ever exceed.
    pub(super) fn rekey_remote_session_if_needed(&mut self, cx: &mut Context<Self>) {
        let Some(agent_id) = self.thread.session_id.clone() else {
            return;
        };
        if agent_id.is_empty() || agent_id == self.remote_session_id {
            return;
        }
        let previous = std::mem::replace(&mut self.remote_session_id, agent_id);
        // `bind_remote` registers under the field just replaced; the old entry is
        // removed afterwards so the two never coexist beyond this call.
        self.bind_remote(cx);
        if let Some(rc) = cx.try_global::<RemoteControl>() {
            rc.rekey_session(&previous, &self.remote_session_id);
        }
    }

    /// (Re)bind this session into the remote-control registry under
    /// [`Self::remote_session_id`]. Always registers (the host core reads the
    /// working flag); the event ring and transcript cost nothing until remote
    /// or local access is on (`RemoteBinding::exposed`). Called on connect and
    /// on respawn.
    pub(super) fn bind_remote(&mut self, cx: &mut Context<Self>) {
        // Deliberately does NOT unregister first. On a respawn this runs again for
        // the same session id, and `register` swaps the backend in place — keeping
        // `seq` monotonic so a subscribed phone keeps receiving. Unregistering here
        // would mint a fresh handle at seq 1, which every subscriber would silently
        // discard as already-seen. Teardown paths still call `unbind_remote`.
        let bound = self
            .connection
            .clone()
            .and_then(|conn| {
                cx.try_global::<RemoteControl>().and_then(|rc| rc.bind(&self.remote_session_id, conn))
            });
        match bound {
            Some(binding) => {
                self.remote = Some(binding);
                // Expose the current transcript at once — on a restart this fold was
                // restored from disk and never entered the event ring, so a phone
                // opening the session before the next event would otherwise see it
                // empty. Meta too, so the row is labelled from the first list.
                self.publish_remote_meta();
                self.publish_remote_transcript();
                // Re-arm the remote-prompt echo relay against the new binding.
                let (tx, rx) = futures::channel::mpsc::unbounded();
                let (event_tx, event_rx) = futures::channel::mpsc::unbounded();
                if let Some(binding) = &self.remote {
                    binding.set_prompt_sink(tx);
                    binding.set_event_sink(event_tx);
                }
                self._remote_prompt_task = Some(Self::spawn_remote_prompt_relay(rx, event_rx, cx));
                // Point the *existing* choice relay at the new binding rather than
                // starting a fresh one. This path runs inside `change_model` →
                // `respawn`, so the relay is mid-flight on the very pick that
                // caused it: replacing the task here would drop that future and
                // cancel its reply, telling the phone a change it just made had
                // failed. Re-registering the same sender is enough — a rebind may
                // mint a new handle (after an unbind), and this points it back.
                let choice_tx = self.choice_relay_sender(cx);
                if let Some(binding) = &self.remote {
                    binding.set_choice_sink(choice_tx);
                }
            }
            // No connection — drop any prior binding.
            None => self.unbind_remote(),
        }
    }

    /// Drop this session's registry binding (on disconnect / teardown). No-op when
    /// unbound. Explicit because the registry retains its own handle `Arc`, so
    /// dropping the view's handle alone would not evict the session.
    pub(super) fn unbind_remote(&mut self) {
        if let Some(binding) = self.remote.take() {
            binding.unregister(&self.remote_session_id);
        }
    }

    /// Push this tab's title + effective model into the registry so a remote
    /// session list renders them instead of the raw `agent-N` id. No-op when remote
    /// is disabled (`remote` is `None`); the registry skips unchanged values, which
    /// is the common case on a per-batch call.
    pub(super) fn publish_remote_meta(&self) {
        let Some(binding) = &self.remote else {
            return;
        };
        binding.set_meta(SessionMeta {
            // The tab's visible title (a manual rename, else the running label) is
            // what the desktop shows, so it wins; fall back to the thread's
            // provider-native title until the pane group has synced one.
            title: self.remote_tab_title.clone().or_else(|| self.thread.title.clone()),
            model: self.effective_model(),
            permission_mode: self.effective_permission_mode(),
            // Git RPCs resolve their repository from this.
            cwd: Some(self.cwd.clone()),
        });
        // Every event batch passes through here, so the first batch of a turn
        // and its `TurnEnded` both reach the host's lineage subscriber.
        binding.set_working(
            self.thread.turn_active
                || self.thread.pending_permission().is_some()
                || self.thread.pending_question().is_some(),
        );
    }

    /// The model this tab is actually running under.
    ///
    /// The same shape as [`Self::effective_permission_mode`], and for the same
    /// reason: `model` is `None` until the user picks one, so a session that has
    /// never had a pick would otherwise publish nothing and a remote picker would
    /// render every model unselected — as if the session were running on no model
    /// at all. The desktop's own composer resolves it exactly this way, falling
    /// back to the connection's default when the tab holds no pick.
    ///
    /// The thread's negotiated model still wins: once the backend reports what it
    /// actually loaded, that is the truth, and it can differ from the default the
    /// child was launched with.
    pub(super) fn effective_model(&self) -> Option<String> {
        self.thread
            .model
            .clone()
            .or_else(|| self.model.clone())
            .or_else(|| self.connection.as_ref().and_then(|c| c.default_model()))
    }

    /// The permission mode this tab is actually running under.
    ///
    /// `permission_mode` is `None` for the backend's baseline — that is what makes
    /// `respawn` omit the flag — so the field alone cannot say what is in force.
    /// A remote picker needs the resolved answer: it holds no connection of its
    /// own to ask for the baseline.
    pub(super) fn effective_permission_mode(&self) -> Option<String> {
        self.permission_mode
            .clone()
            .or_else(|| self.connection.as_ref().and_then(|c| c.default_mode()))
    }

    /// Record the visible tab title (a manual rename, else the running `Chat N` /
    /// agent label) the desktop shows for this chat, so a remote session list
    /// renders the same name instead of the raw `agent-N` id. Pushed by the owning
    /// pane group on tab create, rename, and ambient title change; re-publishes the
    /// registry meta only when the title actually changed.
    pub fn set_remote_tab_title(&mut self, title: Option<String>) {
        if self.remote_tab_title == title {
            return;
        }
        self.remote_tab_title = title;
        self.publish_remote_meta();
    }

    /// Put this chat's credential in a spawn's environment (every spawn: a
    /// respawned process gets a fresh one and the old one is revoked). Without
    /// one — confinement off, or local access off — the agent keeps the reach
    /// it had before confinement existed.
    ///
    /// With a credential, a Claude agent in the `Default` posture also gets
    /// the delegation tools (`oximux mcp --tools orchestrate`), reading the
    /// credential from its file. A Review or Verify agent never does, as it
    /// gets no computer-use server: it only reads.
    pub(super) fn add_spawn_credential(
        spec: &mut ConnectSpec,
        remote_session_id: &str,
        adapter_id: Option<&str>,
        cx: &App,
    ) {
        let Some(rc) = cx.try_global::<RemoteControl>() else { return };
        let Some(access) = rc.mint_chat_credential(remote_session_id) else { return };
        for (key, value) in &access.env {
            if let Err(err) = spec.set_env(key, value) {
                // A user-configured env var of the same name: the agent runs
                // with what the user set, unconfined, and the log says why.
                tracing::warn!(%err, "chat credential not injected");
            }
        }
        let Some(file) = access.credential_file else { return };
        if !wants_delegation_tools(rc.host_config().expose_tools, spec) {
            return;
        }
        let Some(cli) = crate::platform::bundled_cli::cli_path() else {
            tracing::debug!("no oximux binary to run; the agent gets no delegation tools");
            return;
        };
        if let Err(err) = spec.add_mcp_server(orchestration_server(&cli, &file, adapter_id.unwrap_or("claude-code"))) {
            tracing::warn!(%err, "delegation tools not declared");
        }
    }

    /// The agent process is gone: its credential stops working with it.
    pub(super) fn revoke_spawn_credential(&self, cx: &App) {
        if let Some(rc) = cx.try_global::<RemoteControl>() {
            rc.revoke_chat_credential(&self.remote_session_id);
        }
    }
}

/// Whether a spawn gets the delegation tools: the setting is on, the agent
/// speaks MCP through a host declaration this build wires (Claude), and it is
/// not held to a read-only posture.
pub(super) fn wants_delegation_tools(expose_tools: bool, spec: &ConnectSpec) -> bool {
    expose_tools
        && spec.transport == Transport::StreamJson
        && spec.posture == oximux_agents::thread::launch_posture::SpawnPosture::Default
}

/// The delegation tool server a chat agent starts: the CLI's `mcp`, with the
/// agent's credential file (never the token itself) in its arguments.
pub(super) fn orchestration_server(cli: &std::path::Path, credential: &std::path::Path, agent: &str) -> McpServerSpec {
    McpServerSpec::new("oximux", cli.to_string_lossy()).args(vec![
        "mcp".into(),
        "--tools".into(),
        "orchestrate".into(),
        "--credential-file".into(),
        credential.to_string_lossy().into_owned(),
        "--default-agent".into(),
        agent.into(),
    ])
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use oximux_agents::thread::ChatBackend;
    use oximux_agents::thread::launch_posture::SpawnPosture;

    use super::*;

    fn spec(transport: Transport, posture: SpawnPosture) -> ConnectSpec {
        let backend = ChatBackend { transport, ..ChatBackend::stream_json() };
        let mut spec = ConnectSpec::for_backend(&backend, "/repo".into(), None, None, None, None);
        spec.posture = posture;
        spec
    }

    /// Review and Verify agents only read: no delegation tools, whatever the
    /// setting. Nor does a transport this build does not wire them for.
    #[test]
    fn only_a_default_claude_agent_gets_the_delegation_tools() {
        assert!(wants_delegation_tools(true, &spec(Transport::StreamJson, SpawnPosture::Default)));
        assert!(!wants_delegation_tools(false, &spec(Transport::StreamJson, SpawnPosture::Default)));
        for posture in [SpawnPosture::Review, SpawnPosture::Verify] {
            assert!(!wants_delegation_tools(true, &spec(Transport::StreamJson, posture)), "{posture:?}");
        }
        for transport in [Transport::Acp, Transport::AppServer, Transport::Rpc] {
            assert!(!wants_delegation_tools(true, &spec(transport, SpawnPosture::Default)), "{transport:?}");
        }
    }

    /// The server's command line names the credential file, never the token:
    /// argv is visible to every local user.
    #[test]
    fn the_delegation_server_carries_the_credential_path_not_the_token() {
        let server = orchestration_server(Path::new("/app/bin/oximux"), Path::new("/data/agent-credentials/agent-x"), "claude-code");
        assert_eq!(server.name, "oximux");
        assert_eq!(server.command, "/app/bin/oximux");
        assert_eq!(
            server.args,
            ["mcp", "--tools", "orchestrate", "--credential-file", "/data/agent-credentials/agent-x", "--default-agent", "claude-code"]
        );
        assert!(server.env.is_empty());
    }
}
