//! Test-only constructors and probes for [`AgentChatView`]: a view with an
//! injected connection and no background drain, plus accessors a
//! `#[gpui::test]` reads instead of private state.

use super::*;

impl AgentChatView {
    /// Test-only constructor: inject a connection (a `StubConnection`) instead
    /// of spawning a real subprocess, and skip the background drain so a
    /// `#[gpui::test]` can drive `on_event`/`on_disconnect` synchronously.
    pub(crate) fn with_connection_for_test(
        connection: Arc<dyn AgentConnection>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            ComposerView::new(
                theme,
                density,
                typography.clone(),
                ChatBackend::stream_json().provider_display_name(),
                window,
                cx,
            )
        });
        // Mirror `new`: seed the palette's command metadata from the backend, so
        // a test exercises the same path the real constructor takes.
        push_slash_catalog(Some(connection.as_ref()), &composer, std::path::Path::new(""), cx);
        // No thread yet in this constructor, so the placeholder is correct: a
        // test view has never run and so has no agent session id to key on.
        let remote_session_id = remote_session_id_for(None);
        let remote = cx
            .try_global::<RemoteControl>()
            .and_then(|rc| rc.bind(&remote_session_id, connection.clone()));

        let focus = cx.focus_handle();
        Self {
            thread: ChatThread::new(),
            connection: Some(connection),
            remote_session_id,
            spawn_posture: Default::default(),
            posture_refused: None,
            send_gate: Self::new_send_gate(),
            pending_send_uuid: None,
            pending_respawn: false,
            remote,
            backend: ChatBackend::stream_json(),
            composer,
            session_detail_open: false,
            last_notify: std::time::Instant::now(),
            flush_scheduled: false,
            probed_catalogs: HashMap::new(),
            // The injected StubConnection is the whole point: no subprocess.
            probe_catalogs_live: false,
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
            screen_control: ScreenControl::new(&PathBuf::new()),
            screen_prompts: HashMap::new(),
            cwd: PathBuf::new(),
            model: None,
            permission_mode: None,
            effort: None,
            feature_values: HashMap::new(),
            disconnected: false,
            interrupted: false,
            retry: retry::ChatRetry::default(),
            dormant: false,
            publish_throttle: publish_throttle::PublishThrottle::new(),
            last_saved_revision: std::cell::Cell::new(u64::MAX),
            meta_dirty: std::cell::Cell::new(false),
            // The test injects a live connection, so this chat is already bound.
            unbound: false,
            unbound_agent_id: None,
            view_mode: ChatViewMode::Chat,
            terminal: None,
            companion_session: None,
            chat_advanced_since_companion: false,
            companion_spawn_pending: false,
            _terminal_observer: None,
            expanded_thinking: HashSet::new(),
            collapsed_thinking: HashSet::new(),
            thinking_level: ThinkingLevel::default(),
            expanded_tool_calls: HashSet::new(),
            expanded_tool_runs: HashSet::new(),
            image_cache: ImageCache::new(),
            preview: None,
            forge_picker: None,
            forge_picker_gen: 0,
            _forge_task: None,
            open_tool_sheet: None,
            sheet_copied: false,
            _sheet_copy_task: None,
            _drain_task: None,
            drain_gen: 0,
            _remote_prompt_task: None,
            _remote_choice_task: None,
            remote_choice_tx: None,
            _subscriptions: Vec::new(),
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
            is_git_project: false,
            worktree_draft_enabled: false,
            worktree_slug_input: None,
            _worktree_slug_sub: None,
            worktree_create_state: roster::WorktreeCreateState::default(),
            pending_worktree_send: None,
            worktree_branch_label: None,
            import_bridge: None,
        }
    }

    /// Test-only: whether an agent connection is live behind this view. Lets a
    /// test assert the chat is usable, rather than inferring it from a method
    /// that would consume the connection to look.
    pub(super) fn has_connection_for_test(&self) -> bool {
        self.connection.is_some()
    }

    /// Test-only: put this view into the unbound *New Agent* draft state (no
    /// connection, Claude picked) so a `#[gpui::test]` can drive `change_agent` /
    /// `change_model` on a draft without spawning a subprocess.
    pub(super) fn make_unbound_for_test(&mut self) {
        self.connection = None;
        self.unbound = true;
        self.unbound_agent_id = Some("claude-code".to_string());
        self.backend = ChatBackend::stream_json();
        self.model = Some("opus".to_string());
    }

    /// Test-only: the inverse of [`Self::make_unbound_for_test`] — mark the draft
    /// as bound the way a successful first send does, without spawning anything.
    /// The stub connection the test harness injects stands in for the real one.
    pub(super) fn make_bound_for_test(&mut self) {
        self.unbound = false;
        self.unbound_agent_id = None;
    }

    /// Test-only: override `is_git_project` — the real constructor derives it
    /// from a `.git` stat on `cwd`, which a `#[gpui::test]`'s throwaway path
    /// never has, so tests that need the worktree-toggle to render set it here.
    pub(super) fn set_git_project_for_test(&mut self, is_git: bool) {
        self.is_git_project = is_git;
    }

    pub(super) fn worktree_draft_enabled_for_test(&self) -> bool {
        self.worktree_draft_enabled
    }

    pub(super) fn worktree_create_state_for_test(&self) -> &roster::WorktreeCreateState {
        &self.worktree_create_state
    }

    pub(super) fn backend_transport_for_test(&self) -> Transport {
        self.backend.transport
    }

    pub(super) fn model_for_test(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub(super) fn unbound_agent_id_for_test(&self) -> Option<&str> {
        self.unbound_agent_id.as_deref()
    }

    /// Test-only: whether a subprocess connection exists at all — distinct from
    /// [`Self::is_bound_for_test`], which also requires the view to *know* it's
    /// bound. The gap between the two is exactly the bug `/clear` used to cause.
    pub(super) fn connection_is_live_for_test(&self) -> bool {
        self.connection.is_some()
    }

    pub(super) fn is_bound_for_test(&self) -> bool {
        !self.unbound && self.connection.is_some()
    }
}
