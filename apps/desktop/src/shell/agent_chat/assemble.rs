//! Building a chat view and keeping its composer in step with it.
//!
//! `assemble` is the shared constructor every chat-view flavor funnels
//! through; `sync_composer` pushes connection and turn state back down into
//! the composer it wired. Both sit on the same view/composer seam.
//!
//! Lifted out of `agent_chat/mod.rs` so that file can ratchet back under the
//! size cap. A pure move: neither method changed, and both stay inherent
//! methods on `AgentChatView` (an inherent impl may live in any module of the
//! crate, and a child module sees its parent's private items).

use super::*;

impl AgentChatView {
    /// Shared construction for every chat-view flavor: wire the composer,
    /// spawn the subprocess when the mode says so (resuming when
    /// `thread.session_id` is set), and start the event drain. A spawn
    /// failure degrades to a read-only error state so the tab still opens
    /// and explains what went wrong.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn assemble(
        cwd: PathBuf,
        model: Option<String>,
        backend: ChatBackend,
        mut thread: ChatThread,
        mode: ConnectMode,
        // A restored chat's persisted, backend-specific posture, seeded into the
        // connection spawn + the composer's feature picks so the reopened session
        // keeps the choice it was saved with. Empty for fresh launches.
        posture: RestoredPosture,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Cheap, sync `.git` stat (not an async `Repository::open`) — this
        // only gates whether the *New Agent* draft's worktree toggle renders
        // at all, run on every construction so it's ready before first paint.
        let is_git_project = cwd.join(".git").exists();
        let composer = cx.new(|cx| {
            ComposerView::new(
                theme,
                density,
                typography.clone(),
                backend.provider_display_name(),
                window,
                cx,
            )
        });
        // The composer owns its input and repaints itself per keystroke. We only
        // react when it reports a finished submission — so typing never touches
        // this view (and thus never rebuilds the transcript, which is the lag we
        // want to avoid).
        // `subscribe_in` rather than `subscribe`: the worktree pick has to create
        // or drop the slug `InputState`, which needs a `Window`. Every other arm
        // ignores it.
        let subscriptions = vec![cx.subscribe_in(
            &composer,
            window,
            |this, _composer, ev: &ComposerEvent, window, cx| match ev {
                ComposerEvent::Submit { text, images } => {
                    // A staged edit reroutes: rewind to the edited message, then
                    // send the edited text into the forked session.
                    if this.pending_edit.is_some() {
                        this.send_pending_edit(text.clone(), images.clone(), cx)
                    } else {
                        this.send_text(text.clone(), images.clone(), cx)
                    }
                }
                ComposerEvent::SteerNow { text } => this.steer_text(text.clone(), cx),
                ComposerEvent::Stop => this.stop_turn(cx),
                ComposerEvent::ModelPicked(model) => this.change_model(model.clone(), cx),
                ComposerEvent::PermissionModePicked(mode) => {
                    this.change_permission_mode(mode.clone(), cx)
                }
                ComposerEvent::EffortPicked(effort) => this.change_effort(effort.clone(), cx),
                ComposerEvent::ThinkingDisplayPicked(wire) => {
                    this.set_thinking_display_level(wire, cx)
                }
                ComposerEvent::FeaturePicked { id, value } => {
                    this.change_feature(id.clone(), value.clone(), cx)
                }
                ComposerEvent::AgentPicked(id) => this.change_agent(id.clone(), cx),
                ComposerEvent::WorktreeIsolationPicked(enabled) => {
                    this.set_worktree_isolation(*enabled, window, cx)
                }
                ComposerEvent::MentionOpened => this.refresh_context_sources(cx),
                ComposerEvent::CaptureContext(request) => {
                    this.capture_context(request.clone(), cx)
                }
            },
        )];

        // A resumed thread carries the prior session id; a fresh one is `None`
        // (spawn a new session). Either way the subprocess is spawned the same.
        let resume_session_id = thread.session_id.clone();
        let mut connection: Option<Arc<dyn AgentConnection>> = None;
        let mut disconnected = false;
        let mut drain_task = None;
        let screen_control = ScreenControl::new(&cwd);
        // A fresh/restored session always starts in the default permission mode
        // (see the `permission_mode` field note); a live switch respawns.
        // Only an eager chat spawns here. An unbound draft binds via
        // `respawn()` on the first send; a dormant restore connects on first
        // render / remote open; a bridge never connects.
        if mode == ConnectMode::Connect {
            let mut spec = ConnectSpec::for_backend(
                &backend,
                cwd.clone(),
                model.clone(),
                resume_session_id.clone(),
                None,
                None,
            );
            // A restored chat resumes under its persisted posture. For Pi this is
            // the only tool gate there is, so losing it would silently widen the
            // session to the (permissive) default.
            spec.codex_posture = posture.codex.clone();
            spec.pi_posture = posture.pi.clone();
            spec.omp_posture = posture.omp;
            spec.claude_fast_mode =
                claude_fast_mode_to_apply(posture.claude_fast_mode, model.as_deref());
            match computer_use::connect_declaring(spec, &screen_control, cx) {
                Ok((conn, rx)) => {
                    connection = Some(conn);
                    drain_task = Some(Self::spawn_drain(rx, cx));
                }
                Err(e) => {
                    thread.last_error = Some(format!("Failed to start agent: {e}"));
                    disconnected = true;
                }
            }
        }

        // Seed the context meter from the backend when it knows its window
        // without having run a turn (Pi reports it per model at the handshake).
        // Without this the meter has no denominator until the first reply lands.
        // `.or()` keeps a restored transcript's cached window when the backend
        // has nothing to offer.
        thread.last_known_context_window =
            connection.as_ref().and_then(|c| c.context_window()).or(thread.last_known_context_window);

        // Seed the composer's bottom-toolbar pickers now, so they're correct on
        // the very first paint — a restored chat that isn't streaming fires no
        // event, so `sync_composer` wouldn't otherwise run until the next turn
        // (and the capability-gated pickers would be missing until then).
        // Permission mode + effort both start unset (the CLI defaults apply).
        let caps = connection
            .as_ref()
            .map(|c| c.capabilities())
            .unwrap_or_default();
        let vocab = control_vocab_of(connection.as_deref());
        // Seed the palette from the rehydrated list so a restored chat offers it
        // on the first paint — `--resume` stays silent until the first message,
        // so no init would otherwise arrive to populate it.
        let seed_slash = if caps.supports_slash { thread.slash_commands.clone() } else { Vec::new() };
        // Seed ↑/↓ prompt history from the restored transcript's user prompts
        // (oldest→newest) so a resumed chat can recall what was already sent.
        let history_seed: Vec<String> = thread
            .entries
            .iter()
            .filter_map(|e| match e {
                ThreadEntry::User { text, .. } if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            })
            .collect();
        // A restored bound chat with a session can open its terminal view right
        composer.update(cx, |c, cx| {
            c.set_state(disconnected, thread.turn_active, cx);
            c.set_can_steer(caps.supports_steer, cx);
            c.set_controls(model.clone(), None, None, caps.supports_modes, caps.supports_config, vocab, cx);
            // Descriptions + hints aren't persisted — a restored session shows
            // names only until the live agent re-advertises via SlashCommandsUpdated.
            c.set_slash_commands(
                seed_slash,
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
                cx,
            );
            c.seed_history(history_seed);
        });

        push_slash_catalog(connection.as_deref(), &composer, &cwd, cx);

        // Resolve the git checkpoint engine for `cwd` off-thread (it shells out
        // to `git rev-parse`). Folds into `checkpoint_engine` when ready; a
        // non-repo cwd or old git leaves it `None` (rewind offers conversation
        // -only). Runs on the tokio runtime like the mention scan.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let engine_cwd = cwd.clone();
            let (tx, rx) =
                tokio::sync::oneshot::channel::<Option<oximux_git::checkpoint::CheckpointEngine>>();
            handle.spawn(async move {
                let engine = oximux_git::checkpoint::CheckpointEngine::new(&engine_cwd)
                    .await
                    .ok()
                    .flatten();
                let _ = tx.send(engine);
            });
            cx.spawn(async move |this, cx| {
                if let Ok(Some(engine)) = rx.await {
                    let _ = this.update(cx, |this, _cx| {
                        this.checkpoint_engine = Some(Arc::new(engine));
                    });
                }
            })
            .detach();
        }

        // Scan the project's files once for `@file` mention autocomplete. `rg`
        // runs on the tokio runtime (not gpui's executor), so hop through the
        // tokio handle like the terminal composer does, then fold the list back in
        // on the UI thread. Missing `rg` / no runtime degrades to an empty list.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let scan_root = cwd.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<Vec<String>>();
            handle.spawn(async move {
                let files =
                    crate::shell::compose_bar::mention_resolver::scan_candidates(scan_root).await;
                let _ = tx.send(files);
            });
            cx.spawn(async move |this, cx| {
                if let Ok(files) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        this.composer.update(cx, |c, cx| c.set_mention_candidates(files, cx));
                    });
                }
            })
            .detach();
        }

        // The unbound draft's initial agent id, derived from the seed backend's
        // transport (a New Agent draft defaults to Claude; the composer's agent
        // picker can switch it before the first send). `None` when bound.
        let unbound_agent_id = (mode == ConnectMode::UnboundDraft).then(|| {
            match backend.transport {
                Transport::StreamJson => "claude-code",
                Transport::AppServer => "codex",
                Transport::Acp => "cursor",
                Transport::Rpc => "pi",
                Transport::OmpRpc => "omp",
            }
            .to_string()
        });

        // Keyed on the agent's own session id when the thread has one — a
        // restored chat always does — so a remote client's reference to this
        // session survives the restart that just rebuilt this view.
        let remote_session_id = remote_session_id_for(thread.session_id.as_deref());
        // Bind this session into the remote-control registry now if remote control
        // is enabled (else `None` — nothing registered, nothing teed).
        let remote = connection.clone().and_then(|conn| {
            cx.try_global::<RemoteControl>().and_then(|rc| rc.bind(&remote_session_id, conn))
        });
        // A restored transcript loads at construction (never through the live event
        // drain that publishes for a running session), so publish its meta + folded
        // history now. Without this a remote client opening an idle restored session
        // sees it unlabelled and empty until the next live event — the exact gap on a
        // host restart. `bind_remote` covers the later respawn/reconnect path; this
        // covers cold restore, where `self` does not exist yet so the binding is
        // driven directly from the locals that seed the struct below.
        let mut remote_prompt_task = None;
        let mut remote_choice_task = None;
        let mut remote_choice_sender = None;
        if let Some(binding) = &remote {
            let model = thread.model.clone().or_else(|| model.clone());
            binding.set_meta(SessionMeta {
                title: thread.title.clone(),
                // The backend's baseline again, for the same reason as the mode
                // below: a session opened remotely carries no pick of its own, so
                // without this its picker would show nothing selected until the
                // user changed something. Mirrors `Self::effective_model`, which
                // takes over from the first live republish.
                model: model
                    .clone()
                    .or_else(|| connection.as_ref().and_then(|c| c.default_model())),
                // The backend's baseline: the tab seeds `permission_mode: None`
                // below, and a restored pick republishes when it is applied.
                permission_mode: connection.as_ref().and_then(|c| c.default_mode()),
                cwd: Some(cwd.clone()),
            });
            if let Ok(entries_json) = serde_json::to_string(&thread.entries) {
                binding.publish_transcript(entries_json, model);
            }
            // Relay remotely-injected prompts (phone sends) into this tab so the
            // desktop shows the user's own bubble, not just the reply.
            let (tx, rx) = futures::channel::mpsc::unbounded();
            binding.set_prompt_sink(tx);
            let (event_tx, event_rx) = futures::channel::mpsc::unbounded();
            binding.set_event_sink(event_tx);
            remote_prompt_task = Some(Self::spawn_remote_prompt_relay(rx, event_rx, cx));
            // Complete model/mode picks the backend fixes at spawn, which only
            // this view can carry out (it owns the respawn).
            let (choice_tx, choice_rx) = futures::channel::mpsc::unbounded();
            binding.set_choice_sink(choice_tx.clone());
            remote_choice_sender = Some(choice_tx);
            remote_choice_task = Some(Self::spawn_remote_choice_relay(choice_rx, cx));
        }

        let focus = cx.focus_handle();
        Self {
            thread,
            connection,
            remote_session_id,
            remote,
            backend,
            composer,
            session_detail_open: false,
            last_notify: std::time::Instant::now(),
            flush_scheduled: false,
            focus_handle: focus.clone(),
            scroll: transcript::ScrollState::new(),
            markdown: markdown_state::Markdown::new(cx.entity_id(), focus),
            stick_to_bottom: true,
            // Kick the follow so a restored transcript (which loads at
            // construction, not via `on_event`) is pinned to the true bottom
            // once its async markdown layout settles.
            follow_frames: FOLLOW_FRAMES,
            last_max_offset: 0.0,
            theme,
            density,
            typography,
            screen_control,
            screen_prompts: HashMap::new(),
            cwd,
            model,
            permission_mode: None,
            effort: None,
            // Seed the composer's posture picks from the restored blob so they
            // display (and re-persist) the resumed choice.
            feature_values: seed_posture_feature_values(&posture),
            disconnected,
            // Dormant restores boot resumable-idle: a send respawns via --resume.
            interrupted: mode == ConnectMode::DormantResume,
            dormant: mode == ConnectMode::DormantResume,
            publish_throttle: publish_throttle::PublishThrottle::new(),
            last_saved_revision: std::cell::Cell::new(u64::MAX),
            meta_dirty: std::cell::Cell::new(false),
            unbound: mode == ConnectMode::UnboundDraft,
            unbound_agent_id,
            probed_catalogs: HashMap::new(),
            probe_catalogs_live: true,
            view_mode: ChatViewMode::Chat,
            terminal: None,
            companion_session: None,
            chat_advanced_since_companion: false,
            _terminal_observer: None,
            expanded_thinking: HashSet::new(),
            collapsed_thinking: HashSet::new(),
            thinking_level: ThinkingLevel::default(),
            expanded_tool_calls: HashSet::new(),
            expanded_tool_runs: HashSet::new(),
            image_cache: ImageCache::new(),
            preview: None,
            open_tool_sheet: None,
            sheet_copied: false,
            _sheet_copy_task: None,
            _drain_task: drain_task,
            _remote_prompt_task: remote_prompt_task,
            _remote_choice_task: remote_choice_task,
            remote_choice_tx: remote_choice_sender,
            _subscriptions: subscriptions,
            question_cards: HashMap::new(),
            question_card_subs: HashMap::new(),
            embedded_terminals: HashMap::new(),
            embedded_terminal_subs: HashMap::new(),
            env_inputs: Vec::new(),
            env_input_subs: Vec::new(),
            auth: None,
            checkpoint_engine: None,
            pre_turn_checkpoint: None,
            rewind_confirm: None,
            rewinding: false,
            rewind_then_send: None,
            pending_edit: None,
            pane_group: None,
            remote_tab_title: None,
            show_background_tasks: false,
            flash_entry: None,
            flash_frames: 0,
            drop_hint: None,
            recently_copied: None,
            _copied_clear_task: None,
            rows: RefCell::new(Vec::new()),
            find_bar: None,
            rail_hover: false,
            menu_hover: false,
            title_generated: false,
            title_task: None,
            is_git_project,
            worktree_draft_enabled: false,
            worktree_slug_input: None,
            _worktree_slug_sub: None,
            worktree_create_state: roster::WorktreeCreateState::default(),
            pending_worktree_send: None,
            worktree_branch_label: None,
            import_bridge: None,
        }
    }

    /// Push the current connection/turn state + session controls into the
    /// composer so its status line, Send button, and bottom-toolbar pickers all
    /// reflect reality. Cheap no-op when nothing changed (both setters guard).
    pub(super) fn sync_composer(&self, cx: &mut Context<Self>) {
        // A rewind in flight — or a pending ACP auth prompt (the session can't
        // accept input until the user signs in) — disables the composer just like
        // a disconnect until it resolves. A worktree create in flight (or one
        // that failed with a message still staged) folds in the same way: the
        // composer's own `submit()` short-circuits on `disconnected` before it
        // ever emits a second `Submit`, which is what keeps a second distinct
        // send from falling through `send_text`'s `bind_now` at the ORIGINAL
        // cwd while the worktree step is still pending (HIGH finding).
        let worktree_busy = !matches!(self.worktree_create_state, roster::WorktreeCreateState::Idle);
        let (disconnected, turn_active) = (
            self.disconnected || self.rewinding || self.auth.is_some() || worktree_busy,
            self.thread.turn_active,
        );
        // Advertise controls by capability, not by hard-coding the provider.
        let caps = self
            .connection
            .as_ref()
            .map(|c| c.capabilities())
            .unwrap_or_default();
        let mut vocab = control_vocab_of(self.connection.as_deref());
        // Overlay optimistic feature picks so a toggle/select reflects the user's
        // choice immediately, without waiting for the backend to echo it back.
        apply_feature_overrides(&mut vocab.features, &self.feature_values);
        let (model, permission_mode, effort) =
            (self.model.clone(), self.permission_mode.clone(), self.effort.clone());
        // The command palette is offered only when the backend advertises
        // commands (Claude does; others send an empty list, which disables it).
        let slash_commands =
            if caps.supports_slash { self.thread.slash_commands.clone() } else { Vec::new() };
        let slash_descriptions = if caps.supports_slash {
            self.thread.slash_command_descriptions.clone()
        } else {
            std::collections::HashMap::new()
        };
        let slash_hints = if caps.supports_slash {
            self.thread.slash_command_hints.clone()
        } else {
            std::collections::HashMap::new()
        };
        // The input placeholder follows the bound agent ("Message Codex…"); a New
        // Agent draft that just bound gets its real provider name here (it was
        // constructed with the generic "Agent" placeholder).
        let provider_label = self.provider_label().to_string();
        // Live context-meter inputs: prefer the mid-turn `live_usage`, fall back
        // to the settled `usage`; total token occupancy = input + cache + output
        // (ACP folds its whole "used" count into `input_tokens`). The window is
        // the cross-turn cached denominator; cost is the session accumulator.
        let meter_used = self
            .thread
            .live_usage
            .as_ref()
            .or(self.thread.usage.as_ref())
            .map(|u| {
                u.input_tokens + u.cache_read_tokens + u.cache_creation_tokens + u.output_tokens
            });
        let meter_window = self.thread.last_known_context_window;
        let meter_cost = self.thread.session_cost_usd;
        // An unbound draft has no `connection`, so `caps`/`vocab` above are the
        // *empty* defaults — pushing them would blank the draft's pre-bind model
        // list. Its picker shape is owned by `sync_unbound_composer` instead.
        let unbound = self.unbound;
        // Thinking-visibility chip: a transcript view preference, surfaced only
        // once the transcript actually holds a thinking block — before that
        // there is nothing the control could change, so it stays hidden.
        let thinking_display = self
            .thread
            .entries
            .iter()
            .any(|e| matches!(e, ThreadEntry::Assistant(m) if !m.thinking.is_empty()))
            .then(|| self.thinking_level.wire().to_string());
        self.composer.update(cx, |c, cx| {
            c.set_state(disconnected, turn_active, cx);
            c.set_can_steer(caps.supports_steer, cx);
            c.set_usage_meter(meter_used, meter_window, meter_cost, cx);
            c.set_slash_commands(slash_commands, slash_descriptions, slash_hints, cx);
            c.set_provider_label(provider_label, cx);
            c.set_thinking_display(thinking_display, cx);
            if !unbound {
                c.set_controls(model, permission_mode, effort, caps.supports_modes, caps.supports_config, vocab, cx);
                // A bound chat never shows the agent picker (its transport is
                // fixed) or the worktree pill (its cwd is fixed); clearing both
                // here is what hides them after `bind_now` (cheap no-op once
                // already cleared). The pill is pushed from
                // `sync_unbound_composer`, which stops running once bound — so
                // without this clear the composer would keep rendering the stale
                // draft against a live session.
                c.set_agent_picker(false, Vec::new(), None, cx);
                c.set_worktree_draft(None, cx);
            }
        });
        // The composer keeps its own `unbound` flag, and the agent picker, the
        // Import-session row and the placeholder's agent name all read it. Any
        // sync while the draft is still unbound must therefore re-assert the
        // draft's shape rather than the bound-chat shape, or flipping an
        // unrelated control (the worktree toggle syncs here) silently strips
        // those three from a New Agent draft with no way to get them back.
        //
        // This and the `if !unbound` guard above are INDEPENDENT safety nets:
        // either one alone repairs the symptom today, so neither is redundant in
        // the sense of being deletable. The guard stops a connection-less draft's
        // empty vocab being pushed at all; this re-asserts the real shape for
        // every one of `sync_composer`'s callers rather than just the ones that
        // happen to seed it. Removing either leaves the invariant resting on a
        // single accident of ordering.
        if unbound {
            self.sync_unbound_composer(cx);
        }
    }
}
