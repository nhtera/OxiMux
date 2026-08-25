//! View-level tests for the agent chat: extracted verbatim from the tail
//! of `mod.rs` (the module was inline there) so the view file stays under
//! its file-size ratchet budget. Same module path — `tests` is still a
//! child of `agent_chat`, so private access and test filter names are
//! unchanged.

    use super::*;
    use gpui::TestAppContext;
    use oximux_agents::thread::connection::AgentCapabilities;
    use oximux_agents::thread::StubConnection;
    use serde_json::json;

    /// An optimistic feature pick overlays the backend-advertised value so the
    /// control reflects the user's choice immediately (a toggle flips, a select
    /// re-points) — and an override for an id the backend no longer advertises is
    /// harmlessly ignored.
    #[test]
    fn apply_feature_overrides_overlays_picks() {
        use oximux_agents::thread::{FeatureControl, FeatureKind, FeatureSelectOption};
        let mut features = vec![
            FeatureControl {
                id: "fast".into(),
                label: "Fast".into(),
                description: None,
                icon: None,
                kind: FeatureKind::Toggle { on: false },
            },
            FeatureControl {
                id: "mode".into(),
                label: "Session Mode".into(),
                description: None,
                icon: None,
                kind: FeatureKind::Select {
                    options: vec![
                        FeatureSelectOption { wire: "a".into(), label: "A".into(), description: None },
                        FeatureSelectOption { wire: "b".into(), label: "B".into(), description: None },
                    ],
                    selected: Some("a".into()),
                },
            },
        ];
        let overrides = HashMap::from([
            ("fast".to_string(), FeatureValue::Bool(true)),
            ("mode".to_string(), FeatureValue::Choice("b".into())),
            ("stale".to_string(), FeatureValue::Bool(true)), // no matching feature → ignored
        ]);
        apply_feature_overrides(&mut features, &overrides);
        assert!(matches!(features[0].kind, FeatureKind::Toggle { on: true }));
        match &features[1].kind {
            FeatureKind::Select { selected, .. } => assert_eq!(selected.as_deref(), Some("b")),
            _ => panic!("expected select"),
        }
    }

    /// A completed catalog probe must never blank a good seed: an empty or failed
    /// revalidation of a disk-seeded picker keeps the seed; only a non-empty
    /// success is adopted and cached. (Regression: the `Ok(empty)` arm once
    /// clobbered a good seed, hiding the picker mid-draft.)
    #[test]
    fn fold_probe_result_preserves_a_good_seed() {
        use oximux_agents::thread::ModelChoice;
        let full = ProbedCatalog {
            models: vec![ModelChoice { wire: "m".into(), label: "m".into(), description: None }],
            default_model: None,
        };
        let empty = ProbedCatalog::default();

        // Non-empty success → adopt it AND hand it back for caching.
        let (state, cache) = fold_probe_result(false, Ok(full.clone()));
        assert!(matches!(state, Some(ProbeState::Ready(ref c)) if !c.models.is_empty()));
        assert_eq!(cache, Some(full));

        // Empty success WITH a good seed → keep the seed (no change, not cached).
        let (state, cache) = fold_probe_result(true, Ok(empty.clone()));
        assert!(state.is_none(), "empty revalidation must not clobber a good seed");
        assert!(cache.is_none());

        // Empty success WITHOUT a seed → adopt empty (agent has no models); not cached.
        let (state, cache) = fold_probe_result(false, Ok(empty));
        assert!(matches!(state, Some(ProbeState::Ready(ref c)) if c.models.is_empty()));
        assert!(cache.is_none(), "an empty catalog is never cached");

        // Error WITH a good seed → keep the seed.
        let (state, _) = fold_probe_result(true, Err(anyhow::anyhow!("boom")));
        assert!(state.is_none(), "a probe error must not clobber a good seed");

        // Error WITHOUT a seed → Failed.
        let (state, _) = fold_probe_result(false, Err(anyhow::anyhow!("boom")));
        assert!(matches!(state, Some(ProbeState::Failed)));
    }

    #[gpui::test]
    async fn disconnect_fails_closed_pending_permission(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("edit notes");
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({"file_path": "notes.txt"}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "notes.txt".into(),
                    suggestions: vec![],
                    kind: oximux_agents::thread::PermissionKind::Tool,
                });
                assert!(
                    view.thread.pending_permission().is_some(),
                    "permission pending before disconnect"
                );

                view.on_disconnect(cx);

                assert!(
                    view.thread.pending_permission().is_none(),
                    "fail-closed clears the pending permission"
                );
                assert!(view.disconnected, "view marks itself disconnected");
            })
            .expect("window update");

        // Best-effort deny reached the (stub) connection.
        let sent = stub_probe.sent();
        assert!(
            sent.iter()
                .any(|s| s["response"]["response"]["behavior"] == "deny"),
            "disconnect must send a deny control_response, got {sent:?}"
        );
    }

    /// The gap this closes: a session's remote id used to be a per-process
    /// counter, so `agent-3` named a different conversation on every launch and a
    /// phone holding one after a restart pointed at whatever was built third.
    /// Once the agent mints its own id, the session moves onto it.
    #[gpui::test]
    async fn a_session_is_rekeyed_onto_the_agent_id_it_is_given(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            let rc = crate::remote_control::RemoteControl::new();
            rc.set_enabled(true);
            cx.set_global(rc);
        });
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let placeholder = window
            .update(cx, |view, _window, _cx| view.remote_session_id().to_string())
            .unwrap();
        assert!(
            placeholder.starts_with("agent-"),
            "a chat that has never run has only a placeholder, got {placeholder}",
        );

        window
            .update(cx, |view, _window, cx| {
                // The agent mints its id, then anything at all arrives.
                view.thread.session_id = Some("11111111-2222-3333-4444-555555555555".into());
                view.on_event(ThreadEvent::AssistantText("hi".into()), cx);

                assert_eq!(
                    view.remote_session_id(),
                    "11111111-2222-3333-4444-555555555555",
                    "the session moved onto the id that names the conversation",
                );
            })
            .unwrap();

        cx.update(|cx| {
            let rc = cx.global::<crate::remote_control::RemoteControl>();
            assert!(
                rc.registry().get("11111111-2222-3333-4444-555555555555").is_some(),
                "and is reachable under it",
            );
            assert!(
                rc.registry().get(&placeholder).is_none(),
                "while the placeholder is gone, so the list shows one session not two",
            );
        });
    }

    /// A normal streamed turn folds into user + assistant entries via `on_event`.
    #[gpui::test]
    async fn on_event_builds_transcript(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("hi");
                view.on_event(ThreadEvent::AssistantText("Hello!".into()), cx);
                view.on_event(
                    ThreadEvent::TurnEnded {
                        result: Some("Hello!".into()),
                        usage: None,
                        is_error: false, turn_diff: None },
                    cx,
                );
                assert_eq!(view.thread.entries.len(), 2, "user + assistant");
                assert!(!view.thread.turn_active, "turn ended");
            })
            .expect("window update");
    }

    /// A burst of streamed deltas folds into the transcript in full while
    /// costing one throttled repaint, not one per token.
    #[gpui::test]
    async fn delta_batch_concatenates_text_and_defers_one_repaint(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("hi");
                // A fresh view has just painted, so the budget isn't up yet and
                // this batch must defer rather than paint.
                view.last_notify = std::time::Instant::now();
                let batch: Vec<ThreadEvent> = (0..10)
                    .map(|i| ThreadEvent::AssistantTextDelta(format!("tok{i} ")))
                    .collect();
                view.apply_batch(batch, cx);

                assert!(
                    view.flush_scheduled,
                    "an all-delta batch inside the interval queues a trailing repaint"
                );
                // Every delta is applied regardless — only the paint waits.
                let text = match view.thread.entries.last() {
                    Some(oximux_agents::thread::ThreadEntry::Assistant(m)) => m.text.clone(),
                    other => panic!("expected a streaming assistant entry, got {other:?}"),
                };
                assert_eq!(
                    text, "tok0 tok1 tok2 tok3 tok4 tok5 tok6 tok7 tok8 tok9 ",
                    "every delta lands, in order — throttling the paint must not drop or reorder text"
                );
            })
            .expect("window update");

        // The trailing repaint lands on its own, without another event to carry
        // it — otherwise a turn's final characters would sit invisible.
        cx.executor().advance_clock(NOTIFY_INTERVAL * 2);
        cx.run_until_parked();
        window
            .update(cx, |view, _window, _cx| {
                assert!(!view.flush_scheduled, "the trailing repaint fired and cleared the flag");
            })
            .expect("window update");
    }

    /// Anything the user can act on paints immediately — a tool card must not
    /// wait behind the streaming throttle.
    #[gpui::test]
    async fn non_delta_in_a_batch_repaints_immediately(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("hi");
                view.last_notify = std::time::Instant::now();
                view.apply_batch(
                    vec![
                        ThreadEvent::AssistantTextDelta("thinking".into()),
                        ThreadEvent::ToolCallStarted {
                            id: "t1".into(),
                            name: "Bash".into(),
                            input: json!({"command": "ls"}),
                        },
                    ],
                    cx,
                );
                assert!(
                    !view.flush_scheduled,
                    "a batch carrying a non-delta paints now, leaving nothing queued"
                );
            })
            .expect("window update");
    }

    /// The multi-line composer splits Enter by the shift modifier: a plain ↵
    /// submits and clears the draft; ⇧↵ falls through to the field as a newline
    /// and does NOT submit; an empty draft submits nothing even on ↵.
    #[gpui::test]
    async fn enter_submits_shift_enter_newlines(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Capture the composer's Submit events (the test constructor wires no
        // subscription, so observe them directly).
        let submits = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        window
            .update(cx, |view, _window, cx| {
                let sink = submits.clone();
                let sub = cx.subscribe(&view.composer, move |_this, _composer, ev, _cx| {
                    if let ComposerEvent::Submit { text, .. } = ev {
                        sink.borrow_mut().push(text.clone());
                    }
                });
                view._subscriptions.push(sub);
            })
            .expect("window update");

        window
            .update(cx, |view, window, cx| {
                view.composer.update(cx, |c, cx| {
                    c.set_draft_for_test("hello", window, cx);
                    // ⇧↵ (shift) is not consumed → falls through to a newline.
                    assert!(
                        !c.on_enter_key(true, window, cx),
                        "Shift+Enter falls through to a newline"
                    );
                    assert_eq!(c.draft_for_test(cx), "hello", "Shift+Enter kept the draft");
                    // Plain ↵ (no shift) submits and clears the draft.
                    assert!(c.on_enter_key(false, window, cx), "Enter is consumed (submit)");
                    assert!(c.draft_for_test(cx).is_empty(), "submit cleared the draft");
                });
            })
            .expect("window update");
        cx.run_until_parked();
        assert_eq!(*submits.borrow(), vec!["hello".to_string()], "only plain Enter submitted");

        // An empty draft never submits, even on ↵ (consumed, but no event).
        window
            .update(cx, |view, window, cx| {
                view.composer.update(cx, |c, cx| {
                    c.set_draft_for_test("", window, cx);
                    assert!(c.on_enter_key(false, window, cx), "Enter is still consumed");
                });
            })
            .expect("window update");
        cx.run_until_parked();
        assert_eq!(submits.borrow().len(), 1, "empty Enter emitted no Submit");
    }

    /// A message submitted while a turn streams is QUEUED (not sent), then
    /// released as a fresh turn when the streaming turn completes — the
    /// composer-parks + parent-drains-on-turn-end loop, end to end.
    #[gpui::test]
    async fn message_submitted_mid_turn_queues_then_sends_on_turn_end(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let count_users = |view: &AgentChatView| {
            view.thread
                .entries
                .iter()
                .filter(|e| matches!(e, ThreadEntry::User { .. }))
                .count()
        };

        window
            .update(cx, |view, window, cx| {
                // Start a turn.
                view.send_text("first".into(), Vec::new(), cx);
                assert!(view.thread.turn_active, "first send started a turn");
                assert_eq!(count_users(view), 1);

                // Submitting now parks the message instead of sending it (the
                // composer sees turn_active via the sync above).
                view.composer.update(cx, |c, cx| {
                    c.set_draft_for_test("second", window, cx);
                    c.submit(window, cx);
                    assert!(c.draft_for_test(cx).is_empty(), "queued submit cleared the draft");
                });
                assert_eq!(count_users(view), 1, "queued, not sent while the turn is active");

                // The turn completes → the queued message is released as a new turn.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                    cx,
                );
                assert_eq!(count_users(view), 2, "queued message sent on turn end");
                assert!(view.thread.turn_active, "the flushed message started a fresh turn");

                // Queue now empty → a second turn end sends nothing more.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                    cx,
                );
                assert_eq!(count_users(view), 2, "no phantom re-send when the queue is empty");
            })
            .expect("window update");
    }

    /// Steering hands a message to the turn that is already streaming: the stub
    /// records a `steer` (not a fresh send), the bubble appears at once, and the
    /// turn keeps running — unlike a normal send, which starts one.
    #[gpui::test]
    async fn steering_feeds_the_live_turn_instead_of_starting_a_new_one(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default().with_capabilities(AgentCapabilities {
            supports_steer: true,
            ..AgentCapabilities::default()
        });
        let recorder = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let count_users = |view: &AgentChatView| {
            view.thread.entries.iter().filter(|e| matches!(e, ThreadEntry::User { .. })).count()
        };

        window
            .update(cx, |view, _window, cx| {
                view.send_text("first".into(), Vec::new(), cx);
                assert!(view.thread.turn_active);

                view.steer_text("actually, stop".into(), cx);
                assert_eq!(count_users(view), 2, "the steered message is in the transcript now");
                assert!(view.thread.turn_active, "the turn it steers is still running");
                assert_eq!(view.thread.last_error, None);

                // Nothing to steer once the turn is over — that message would be
                // an ordinary send, and this path must not fake one.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                    cx,
                );
                view.steer_text("too late".into(), cx);
                assert_eq!(count_users(view), 2, "no bubble for a message that went nowhere");
            })
            .expect("window update");

        let sent = recorder.sent();
        assert_eq!(sent.len(), 2, "the idle steer sent nothing");
        assert_eq!(sent[0]["message"]["content"], "first", "the send that started the turn");
        assert_eq!(sent[1]["type"], "steer", "steered rather than starting a turn");
        assert_eq!(sent[1]["message"], "actually, stop");
    }

    /// A backend with no mid-turn queue refuses the steer, and the refusal is
    /// surfaced rather than swallowed into a bubble the agent never received.
    #[gpui::test]
    async fn a_refused_steer_surfaces_and_pushes_no_bubble(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default(); // supports_steer: false
        let recorder = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.send_text("first".into(), Vec::new(), cx);
                view.steer_text("second".into(), cx);
                assert_eq!(
                    view.thread.entries.iter().filter(|e| matches!(e, ThreadEntry::User { .. })).count(),
                    1,
                    "a message the backend rejected must not render as sent"
                );
                assert!(view.thread.last_error.as_deref().unwrap_or_default().contains("Steer failed"));
            })
            .expect("window update");

        assert_eq!(recorder.sent().len(), 1, "only the send that started the turn");
    }

    /// Picking Read-only must reach the SPAWN, because pi's gating is a
    /// spawn-time allowlist and a respawn is the only thing that applies it.
    ///
    /// This is the round's load-bearing safety property, and it shipped broken:
    /// `respawn` carried `codex_posture` but not `pi_posture`, so the pill read
    /// "Read-only" while pi ran wide open. Live, it wrote `breach.txt` on demand.
    /// The posture is only real if this spec carries it.
    #[gpui::test]
    async fn picking_a_pi_posture_reaches_the_respawn_spec(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, _cx| {
                // A Pi chat that has had its tools pill set to read-only.
                view.backend = ChatBackend::from(Transport::Rpc);
                view.feature_values.insert(
                    pi_posture::FEATURE_TOOLS.to_string(),
                    FeatureValue::Choice(pi_posture::TOOLS_READ_ONLY.to_string()),
                );

                let spec = view.respawn_spec(Vec::new(), None);
                let posture = spec
                    .pi_posture
                    .expect("the respawn must carry the posture, or the pill is decoration");
                assert_eq!(posture.tools, pi_posture::TOOLS_READ_ONLY);
                // And it reaches the child as real argv, not just a struct field.
                let args = oximux_agents::thread::pi::build_args(None, &posture, None)
                    .expect("build argv");
                assert!(
                    args.windows(2).any(|w| w[0] == "--tools"),
                    "read-only must arrive as pi's own allowlist flag: {args:?}"
                );

                // A non-Pi chat carries nothing here (the field is Rpc-only).
                view.backend = ChatBackend::stream_json();
                assert_eq!(view.respawn_spec(Vec::new(), None).pi_posture, None);
            })
            .expect("window update");
    }

    /// Same load-bearing property for omp, with HIGHER stakes on the miss: a
    /// respawn spec that dropped the posture falls to OxiMux's Write default —
    /// and omp's OWN default is yolo, so the flag must both survive the spec
    /// AND always be spelled in the argv (the yolo-default guard, F2).
    #[gpui::test]
    async fn picking_an_omp_posture_reaches_the_respawn_spec(cx: &mut TestAppContext) {
        use oximux_agents::thread::omp::posture::{self as omp_posture, OmpPosture};

        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, _cx| {
                // An omp chat whose Approvals pill was set to Always-ask.
                view.backend = ChatBackend::from(Transport::OmpRpc);
                view.feature_values.insert(
                    omp_posture::FEATURE_APPROVALS.to_string(),
                    FeatureValue::Choice(omp_posture::APPROVAL_ALWAYS_ASK.to_string()),
                );

                let spec = view.respawn_spec(Vec::new(), None);
                let posture = spec
                    .omp_posture
                    .expect("the respawn must carry the posture, or the pill is decoration");
                assert_eq!(posture, OmpPosture::AlwaysAsk);
                // And it reaches the child as real argv — explicitly, because
                // an ABSENT flag is not "the default", it is omp's yolo.
                let args = oximux_agents::thread::omp::build_args(None, &posture, None)
                    .expect("build argv");
                assert!(
                    args.windows(2)
                        .any(|w| w[0] == "--approval-mode" && w[1] == "always-ask"),
                    "always-ask must arrive as omp's own flag: {args:?}"
                );

                // An untouched picker carries None — and even then the spawn
                // path spells the Write default out (see `OmpPosture::to_args`).
                view.feature_values.remove(omp_posture::FEATURE_APPROVALS);
                assert_eq!(view.respawn_spec(Vec::new(), None).omp_posture, None);
                let default_args =
                    oximux_agents::thread::omp::build_args(None, &OmpPosture::default(), None)
                        .expect("build argv");
                assert!(
                    default_args
                        .windows(2)
                        .any(|w| w[0] == "--approval-mode" && w[1] == "write"),
                    "the default must still be explicit: {default_args:?}"
                );

                // A non-omp chat carries nothing here (the field is OmpRpc-only).
                view.backend = ChatBackend::stream_json();
                assert_eq!(view.respawn_spec(Vec::new(), None).omp_posture, None);
            })
            .expect("window update");
    }

    /// A backend that describes its own commands drives the palette's grouping,
    /// descriptions and attribution — no on-disk scan of another CLI's config.
    #[gpui::test]
    async fn a_backends_own_command_metadata_reaches_the_palette(cx: &mut TestAppContext) {
        use oximux_agents::thread::connection::SlashCommandInfo;

        cx.update(gpui_component::init);
        let stub = StubConnection::default()
            .with_capabilities(AgentCapabilities {
                supports_slash: true,
                ..AgentCapabilities::default()
            })
            .with_slash_commands(vec![SlashCommandInfo {
                name: "skill:verify-notes".into(),
                description: Some("Summarize the notes file.".into()),
                is_skill: true,
                source_label: Some("user".into()),
            }]);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.composer.update(cx, |c, _cx| {
                    let cat = c.slash_catalog_for_test();
                    let meta = cat
                        .get("skill:verify-notes")
                        .expect("the backend's own command is in the palette's catalog");
                    assert_eq!(meta.group, super::slash_command_catalog::CommandGroup::Skill);
                    assert_eq!(meta.description.as_deref(), Some("Summarize the notes file."));
                    assert_eq!(meta.source_label.as_deref(), Some("user"));
                    // Nothing from another CLI's on-disk catalog leaked in.
                    assert!(!cat.contains_key("compact"));
                });
            })
            .expect("window update");
    }

    /// Accepting a slash command parks the caret after the inserted `/name `
    /// (not back at offset 0 in the multi-line box) and surfaces the command's
    /// argument hint until an argument is typed.
    #[gpui::test]
    async fn accepting_command_parks_caret_and_shows_arg_hint(cx: &mut TestAppContext) {
        use super::slash_command_catalog::{CommandCatalog, CommandGroup, CommandMeta};

        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.composer.update(cx, |c, cx| {
                    // A backend advertising `git`, enriched with an argument hint
                    // from the on-disk catalog (no backend-advertised hint here).
                    c.set_slash_commands(
                        vec!["git".into(), "compact".into()],
                        std::collections::HashMap::new(),
                        std::collections::HashMap::new(),
                        cx,
                    );
                    let mut cat = CommandCatalog::new();
                    cat.insert(
                        "git".into(),
                        CommandMeta {
                            description: Some("Git operations".into()),
                            argument_hint: Some("cm|cp|pr|merge [args]".into()),
                            group: CommandGroup::BuiltIn,
                            source_label: None,
                        },
                    );
                    c.set_command_catalog(cat, cx);

                    // Type a partial command, open the palette, accept the match.
                    c.set_draft_for_test("/gi", window, cx);
                    c.recompute_overlays_for_test(cx);
                    assert!(c.accept_highlighted_for_test(window, cx), "palette accepted a match");

                    // The whole `/git ` is inserted and the caret sits AFTER the
                    // trailing space — not jumped back to the start of the box.
                    assert_eq!(c.draft_for_test(cx), "/git ");
                    assert_eq!(c.cursor_for_test(cx), "/git ".len());

                    // The argument hint now shows (palette closed by the space).
                    assert_eq!(
                        c.usage_hint_for_test(cx),
                        Some(("git".to_string(), "cm|cp|pr|merge [args]".to_string())),
                    );

                    // A backend-advertised hint (ACP `AvailableCommand.input`) wins
                    // over the on-disk catalog's argument-hint.
                    c.set_slash_commands(
                        vec!["git".into(), "compact".into()],
                        std::collections::HashMap::new(),
                        std::collections::HashMap::from([("git".to_string(), "<subcommand>".to_string())]),
                        cx,
                    );
                    c.recompute_overlays_for_test(cx);
                    assert_eq!(
                        c.usage_hint_for_test(cx),
                        Some(("git".to_string(), "<subcommand>".to_string())),
                        "ACP-advertised hint wins over the catalog argument-hint",
                    );

                    // Typing an argument hides the hint again.
                    c.set_draft_for_test("/git cm", window, cx);
                    c.recompute_overlays_for_test(cx);
                    assert_eq!(c.usage_hint_for_test(cx), None);
                });
            })
            .expect("window update");
    }

    /// Card buttons route Allow/Reject to the connection by request_id and flip
    /// the local status (Allow → InProgress; Deny → Rejected), clearing the
    /// pending prompt. Allow echoes the tool input as updatedInput.
    #[gpui::test]
    async fn approve_and_reject_route_permission_decisions(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("do two things");
                for (tid, rid, name, input) in [
                    ("t1", "r1", "Edit", json!({"file_path": "a.txt"})),
                    ("t2", "r2", "Bash", json!({"command": "rm x"})),
                ] {
                    view.thread.apply(&ThreadEvent::ToolCallStarted {
                        id: tid.into(),
                        name: name.into(),
                        input: input.clone(),
                    });
                    view.thread.apply(&ThreadEvent::PermissionRequested {
                        request_id: rid.into(),
                        tool_use_id: Some(tid.into()),
                        tool_name: name.into(),
                        input,
                        description: name.into(),
                        suggestions: vec![],
                        kind: oximux_agents::thread::PermissionKind::Tool,
                    });
                }

                view.resolve_permission(
                    "t1".into(),
                    "r1".into(),
                    PermissionDecision::Allow { updated_input: json!({"file_path": "a.txt"}) },
                    cx,
                );
                view.resolve_permission(
                    "t2".into(),
                    "r2".into(),
                    PermissionDecision::Deny { message: "no".into() },
                    cx,
                );

                assert!(
                    view.thread.pending_permission().is_none(),
                    "both permissions resolved"
                );
                assert_eq!(tool_status(view, "t1"), Some("InProgress"));
                assert_eq!(tool_status(view, "t2"), Some("Rejected"));
            })
            .expect("window update");

        let sent = stub_probe.sent();
        let allow = sent
            .iter()
            .find(|s| s["response"]["request_id"] == "r1")
            .expect("r1 control_response");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");
        assert_eq!(
            allow["response"]["response"]["updatedInput"],
            json!({"file_path": "a.txt"})
        );
        let deny = sent
            .iter()
            .find(|s| s["response"]["request_id"] == "r2")
            .expect("r2 control_response");
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
    }

    /// Answering an AskUserQuestion routes the selection back as an `allow` whose
    /// `updatedInput` carries the answers map (keyed by question text), settles
    /// the tool locally, and clears the pending question.
    #[gpui::test]
    async fn answer_question_routes_selection_and_settles(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                use oximux_agents::thread::{parse_questions, QuestionAnswer, QuestionAnswers};
                view.thread.push_user_message("choose");
                let input = json!({"questions":[{"question":"Tabs or spaces?","header":"Indent",
                    "options":[{"label":"Tabs","description":""},{"label":"Spaces","description":""}],
                    "multiSelect":false}]});
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "AskUserQuestion".into(),
                    input: input.clone(),
                });
                view.thread.apply(&ThreadEvent::QuestionAsked {
                    request_id: "rq".into(),
                    tool_use_id: Some("t1".into()),
                    questions: parse_questions(&input),
                });
                assert_eq!(tool_status(view, "t1"), Some("AwaitingAnswer"));

                let mut answers = QuestionAnswers::default();
                answers.by_question.insert(
                    "q-0".into(),
                    QuestionAnswer { selected: vec!["Tabs".into()], custom: None },
                );
                view.answer_question("t1".into(), answers, cx);

                assert!(view.thread.pending_question().is_none(), "question answered");
                assert_eq!(tool_status(view, "t1"), Some("InProgress"));
            })
            .expect("window update");

        let sent = stub_probe.sent();
        let ans = sent
            .iter()
            .find(|s| s["response"]["request_id"] == "rq")
            .expect("rq control_response");
        assert_eq!(ans["response"]["response"]["behavior"], "allow");
        assert_eq!(
            ans["response"]["response"]["updatedInput"]["answers"]["Tabs or spaces?"],
            json!("Tabs")
        );
    }

    /// A stray second click after a card is answered must not send a second
    /// control_response or flip the decision — the guard makes it a no-op.
    #[gpui::test]
    async fn second_answer_is_ignored(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("go");
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "x".into(),
                    suggestions: vec![],
                    kind: oximux_agents::thread::PermissionKind::Tool,
                });
                // First answer: allow.
                view.resolve_permission(
                    "t1".into(),
                    "r1".into(),
                    PermissionDecision::Allow { updated_input: json!({}) },
                    cx,
                );
                // Stray second answer: deny — must be ignored (already decided).
                view.resolve_permission(
                    "t1".into(),
                    "r1".into(),
                    PermissionDecision::Deny { message: "no".into() },
                    cx,
                );
                assert_eq!(
                    tool_status(view, "t1"),
                    Some("InProgress"),
                    "stays allowed, not flipped to Rejected by the second click"
                );
            })
            .expect("window update");

        let responses: Vec<_> = stub_probe
            .sent()
            .into_iter()
            .filter(|s| s["response"]["request_id"] == "r1")
            .collect();
        assert_eq!(responses.len(), 1, "exactly one control_response for r1");
        assert_eq!(responses[0]["response"]["response"]["behavior"], "allow");
    }

    /// Stop mid-turn: the turn clears, a pending approval fail-closes, and the
    /// tab enters resumable-idle (interrupted, NOT disconnected — no error).
    #[gpui::test]
    async fn stop_turn_interrupts_and_stays_resumable(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("do a long thing");
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "x".into(),
                    suggestions: vec![],
                    kind: oximux_agents::thread::PermissionKind::Tool,
                });
                assert!(view.thread.turn_active, "turn active before Stop");

                view.stop_turn(cx);

                assert!(!view.thread.turn_active, "Stop ends the turn");
                assert!(view.interrupted, "session marked resumable-idle");
                assert!(!view.disconnected, "an intentional Stop is not a disconnect");
                assert!(
                    view.thread.pending_permission().is_none(),
                    "pending approval fail-closes on Stop"
                );
                assert_eq!(tool_status(view, "t1"), Some("Rejected"));

                // The interrupt `result` arrives flagged as an error; it must be
                // swallowed, not shown as a banner.
                view.on_event(
                    ThreadEvent::TurnEnded {
                        result: None,
                        usage: None,
                        is_error: true, turn_diff: None },
                    cx,
                );
                assert!(
                    view.thread.last_error.is_none(),
                    "the interrupt's error result is suppressed"
                );

                // The child's stdout then EOFs: still resumable, still no error.
                view.on_disconnect(cx);
                assert!(!view.disconnected, "EOF after an intentional Stop stays resumable");
                assert!(view.interrupted);
                assert!(view.thread.last_error.is_none());
            })
            .expect("window update");
    }

    /// Order-independence: if the child's stdout EOF is observed BEFORE the
    /// interrupt's `result` event, the tab must still stay resumable-idle (not
    /// flip to disconnected/unavailable), and a straggler error result arriving
    /// afterward is still suppressed.
    #[gpui::test]
    async fn stop_then_eof_before_result_stays_resumable(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("go");
                view.stop_turn(cx);
                assert!(view.interrupted);

                // EOF arrives first (before any TurnEnded is folded in).
                view.on_disconnect(cx);
                assert!(!view.disconnected, "EOF after Stop stays resumable, order-independent");
                assert!(view.thread.last_error.is_none());

                // A late error result then folds in — still suppressed.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: true, turn_diff: None },
                    cx,
                );
                assert!(view.thread.last_error.is_none());
                assert!(view.interrupted, "still resumable for the next send");
            })
            .expect("window update");
    }

    /// A Stop with no live turn is a no-op (nothing to interrupt).
    #[gpui::test]
    async fn stop_turn_without_active_turn_is_noop(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                assert!(!view.thread.turn_active);
                view.stop_turn(cx);
                assert!(!view.interrupted, "no turn → Stop does nothing");
            })
            .expect("window update");
    }

    /// Rewind race: while a rewind is in flight the connection is taken and the
    /// old child killed, so the old drain task's `on_disconnect` fires on the
    /// foreground racing the rewind's completion. Because `perform_rewind` marks
    /// the kill intentional (`interrupted = true`), that stray `on_disconnect`
    /// must take its resumable-idle branch — NOT strand the tab as disconnected.
    #[gpui::test]
    async fn on_disconnect_during_rewind_does_not_strand_tab(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                // Simulate the state perform_rewind establishes before its
                // background half runs: connection taken, kill marked intentional.
                view.thread.session_id = Some("old-sid".into());
                view.rewinding = true;
                view.interrupted = true;
                view.connection = None;

                // The killed child's stdout EOFs mid-rewind.
                view.on_disconnect(cx);

                assert!(
                    !view.disconnected,
                    "EOF during a rewind must not mark the tab disconnected"
                );
                assert!(
                    view.thread.last_error.is_none(),
                    "no error banner for the rewind's own intentional kill"
                );
                assert!(view.interrupted, "stays resumable-idle for finish_rewind");
            })
            .expect("window update");
    }

    /// With remote control enabled, a chat view registers its session into the
    /// shared registry, tees each applied event to a live subscriber in order, and
    /// evicts the session on disconnect. (The disabled path — no global → no
    /// binding → no clone — is what every other view test exercises implicitly.)
    #[gpui::test]
    async fn remote_enabled_registers_tees_and_unregisters(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            let rc = RemoteControl::new();
            rc.set_enabled(true);
            cx.set_global(rc);
        });
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // The session registered under the view's stable remote id — subscribe.
        let mut rx = window
            .update(cx, |view, _window, cx| {
                cx.global::<RemoteControl>()
                    .registry()
                    .subscribe(&view.remote_session_id)
                    .expect("session registered while remote is enabled")
            })
            .expect("window update");

        // An applied event is teed to the remote subscriber with its assigned seq.
        window
            .update(cx, |view, _window, cx| {
                view.apply_batch(vec![ThreadEvent::AssistantText("hi".into())], cx);
            })
            .expect("window update");
        let (seq, ev) = rx.try_recv().expect("event teed to the remote subscriber");
        assert_eq!(seq, 1, "first teed event gets seq 1");
        assert_eq!(ev, ThreadEvent::AssistantText("hi".into()));

        // A respawn re-binds the SAME id. `seq` must keep climbing and the
        // subscriber must survive — a reset to 1 would look like duplicates to a
        // phone already past that cursor, and it would silently show nothing more.
        window
            .update(cx, |view, _window, cx| {
                view.connection = Some(Arc::new(StubConnection::default()));
                view.bind_remote(cx);
                view.apply_batch(vec![ThreadEvent::AssistantText("after".into())], cx);
            })
            .expect("window update");
        let (seq, ev) = rx.try_recv().expect("the subscription survived the respawn");
        assert_eq!(seq, 2, "seq continues across a respawn instead of resetting");
        assert_eq!(ev, ThreadEvent::AssistantText("after".into()));

        // Disconnect evicts the session from the registry.
        let id = window
            .update(cx, |view, _window, cx| {
                let id = view.remote_session_id.clone();
                view.on_disconnect(cx);
                id
            })
            .expect("window update");
        window
            .update(cx, |_view, _window, cx| {
                assert!(
                    cx.global::<RemoteControl>().registry().get(&id).is_none(),
                    "session unregistered on disconnect",
                );
            })
            .expect("window update");
    }

    /// Regenerate stages the PRECEDING user prompt (unchanged) for re-send via
    /// the rewind machinery — the selection logic that decides *what* re-rolls.
    /// The fork/respawn half is the shared rewind path (covered elsewhere); here
    /// we assert the pick + the idle-only guard without a live async runtime.
    #[gpui::test]
    async fn regenerate_stages_preceding_user_prompt(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                // Regenerate is a rewind-gated (Claude) feature.
                Arc::new(StubConnection::default().with_capabilities(
                    oximux_agents::thread::AgentCapabilities { supports_rewind: true, ..Default::default() },
                )),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                view.thread.session_id = Some("sid".into());
                view.thread.push_user_message("first prompt");
                view.thread.apply(&ThreadEvent::AssistantText("first reply".into()));
                view.thread.push_user_message("second prompt");
                view.thread.apply(&ThreadEvent::AssistantText("second reply".into()));
                view.thread.apply(&ThreadEvent::TurnEnded {
                    result: None,
                    usage: None,
                    is_error: false, turn_diff: None });

                // Guard: while a turn is active, regenerate stages nothing.
                view.thread.turn_active = true;
                let asst_idx = view.thread.entries.len() - 1;
                view.regenerate(asst_idx, cx);
                assert!(view.rewind_then_send.is_none(), "no regenerate mid-turn");
                view.thread.turn_active = false;

                // Regenerating an EARLIER reply (the first, which has a later user
                // turn) is refused — it would silently drop the later turn.
                view.regenerate(1, cx);
                assert!(
                    view.rewind_then_send.is_none(),
                    "regenerate refuses a non-tail reply (later turns would be lost)",
                );

                // Regenerating the last reply stages its owning prompt ("second
                // prompt") unchanged for re-send — not the earlier turn.
                view.regenerate(asst_idx, cx);
                let staged =
                    view.rewind_then_send.as_ref().expect("prompt staged for re-send");
                assert_eq!(staged.0, "second prompt");
                assert!(staged.1.is_empty(), "no images on this prompt");
            })
            .expect("window update");
    }

    /// Staged edit-and-resend must be a TRUE no-op on cancel: entering edit mode
    /// prefills the composer and dims later messages, but Escape/cancel restores
    /// the prior draft and touches neither the transcript nor the session.
    #[gpui::test]
    async fn pending_edit_cancel_is_a_no_op(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                // Edit-and-resend is a rewind-gated (Claude) feature.
                Arc::new(StubConnection::default().with_capabilities(
                    oximux_agents::thread::AgentCapabilities { supports_rewind: true, ..Default::default() },
                )),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                view.thread.session_id = Some("sid".into());
                view.thread.push_user_message("first");
                view.thread.apply(&ThreadEvent::AssistantText("a1".into()));
                view.thread.push_user_message("second");
                view.thread.apply(&ThreadEvent::AssistantText("a2".into()));
                // Edit is only offered on an idle turn (a live turn would queue
                // the resend instead of routing it).
                view.thread.apply(&ThreadEvent::TurnEnded {
                    result: None,
                    usage: None,
                    is_error: false, turn_diff: None });
                let entries_before = view.thread.entries.clone();

                // The user was mid-typing an unrelated draft WITH a staged image.
                let staged = ChatImage {
                    media_type: "image/png".into(),
                    // 1x1 transparent PNG.
                    data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==".into(),
                };
                view.composer.update(cx, |c, cx| {
                    c.prefill("half-typed thought".into(), vec![staged.clone()], window, cx)
                });

                // Edit the FIRST user message (entry index 0).
                view.enter_pending_edit(0, window, cx);
                assert!(view.pending_edit.is_some(), "edit mode entered");
                assert_eq!(
                    view.composer.read(cx).current_draft(cx),
                    "first",
                    "composer prefilled with the edited message"
                );
                assert!(view.is_pending_edit_dimmed(1), "later messages dim");
                assert!(!view.is_pending_edit_dimmed(0), "the edited message itself is not dimmed");

                // Cancel: draft AND staged image restored, nothing removed.
                view.cancel_pending_edit(window, cx);
                assert!(view.pending_edit.is_none(), "edit mode exited");
                assert_eq!(
                    view.composer.read(cx).current_draft(cx),
                    "half-typed thought",
                    "the prior draft is restored verbatim"
                );
                assert_eq!(
                    view.composer.read(cx).current_images(),
                    vec![staged],
                    "the pre-existing staged image is restored (true no-op)"
                );
                assert_eq!(view.thread.entries, entries_before, "transcript untouched");
                assert_eq!(view.thread.session_id.as_deref(), Some("sid"), "session untouched");
            })
            .expect("window update");
    }

    /// A manual collapse during Auto stream auto-expand must register on the
    /// first click (the collapsed override wins over the streaming peek).
    #[gpui::test]
    async fn thinking_chip_pick_sets_level_and_persists(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                assert_eq!(view.thinking_level, ThinkingLevel::Auto, "default is Auto");
                view.meta_dirty.set(false);

                // The composer chip's pick path: wire value in, level applied,
                // persistence marked. An unknown wire must change nothing.
                view.set_thinking_display_level("shown", cx);
                assert_eq!(view.thinking_level, ThinkingLevel::Expanded);
                assert!(view.meta_dirty.get(), "a pick marks the transcript blob dirty");

                view.meta_dirty.set(false);
                view.set_thinking_display_level("bogus", cx);
                assert_eq!(view.thinking_level, ThinkingLevel::Expanded, "unknown wire ignored");
                assert!(!view.meta_dirty.get(), "an ignored pick must not dirty the blob");

                view.set_thinking_display_level("off", cx);
                assert_eq!(view.thinking_level, ThinkingLevel::Hidden);

                // Wire values round-trip so the chip's ✓ always lands on the
                // active row.
                for level in
                    [ThinkingLevel::Hidden, ThinkingLevel::Auto, ThinkingLevel::Expanded]
                {
                    assert_eq!(ThinkingLevel::from_wire(level.wire()), Some(level));
                }
            })
            .unwrap();
    }

    #[gpui::test]
    async fn thinking_manual_collapse_wins_over_auto_stream(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("go");
                // A streaming thought on the last entry, no text yet, turn active.
                view.thread.apply(&ThreadEvent::ThinkingDelta("pondering".into()));
                assert!(view.thread.turn_active);
                let last = view.thread.entries.len() - 1;
                let msg = match &view.thread.entries[last] {
                    ThreadEntry::Assistant(m) => m.clone(),
                    _ => panic!("expected assistant entry"),
                };
                assert!(
                    view.thinking_expanded(last, true, &msg),
                    "Auto auto-expands the streaming thought"
                );

                // One click must collapse it despite the auto-expand.
                view.toggle_thinking(last, cx);
                assert!(
                    !view.thinking_expanded(last, true, &msg),
                    "manual collapse wins on the FIRST click"
                );
                // Toggling again re-expands.
                view.toggle_thinking(last, cx);
                assert!(view.thinking_expanded(last, true, &msg), "re-expands on next click");
            })
            .expect("window update");
    }

    #[test]
    fn a_review_tab_is_keyed_by_diff_content_not_by_position() {
        // The Review tab dedups on this key across the WHOLE pane group, and a
        // match just reactivates the existing tab without reloading it. So a key
        // that two different diffs can share means showing the wrong diff under
        // the right label — silently.
        let turn_a = "diff --git a/a.rs b/a.rs\n+++ b/a.rs\n@@ -0,0 +1 @@\n+a\n";
        let turn_b = "diff --git a/b.rs b/b.rs\n+++ b/b.rs\n@@ -0,0 +1 @@\n+b\n";

        // Two chats' first editing turn both sit at entry index 2; keying on the
        // index would collide here. Keying on content does not.
        assert_ne!(
            diff_tab_key(turn_a),
            diff_tab_key(turn_b),
            "two different turn diffs must never share a Review tab"
        );
        // A rewind repopulates the same index with a different diff — likewise
        // must not reactivate the pre-rewind tab.
        let after_rewind = "diff --git a/a.rs b/a.rs\n+++ b/a.rs\n@@ -0,0 +1 @@\n+a2\n";
        assert_ne!(diff_tab_key(turn_a), diff_tab_key(after_rewind));
        // Reviewing the SAME diff twice reuses its tab — a collision here means
        // the content is identical, so the tab already shows the right thing.
        assert_eq!(diff_tab_key(turn_a), diff_tab_key(turn_a));
    }

    #[test]
    fn tool_grouping_leaves_short_runs_and_messages_alone() {
        // messages interleaved with short tool runs: nothing collapses.
        let is_tool = vec![false, true, true, false, true, false];
        let force = vec![false; 6];
        let plan = plan_tool_grouping(&is_tool, &force, &HashSet::new());
        assert!(plan.iter().all(|d| matches!(d, EntryDisplay::Show)));
    }

    /// A turn that ends in error on a NON-empty transcript records the error and
    /// stays idle — the state the tail error-card arm renders against. Retry
    /// clears the error, re-opens the turn, and re-sends the last prompt (without
    /// pushing a duplicate user bubble).
    #[gpui::test]
    async fn turn_error_surfaces_and_retry_resends_last_prompt(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("do the thing");
                view.thread.apply(&ThreadEvent::TurnEnded {
                    result: Some("API error: overloaded".into()),
                    usage: None,
                    is_error: true, turn_diff: None });

                // Precondition the tail error-card arm keys on: idle, connected,
                // non-empty transcript, an error recorded — and nothing sent yet.
                assert!(!view.thread.turn_active, "turn settled");
                assert!(!view.disconnected, "still connected");
                assert!(!view.thread.entries.is_empty(), "transcript non-empty");
                assert_eq!(
                    view.thread.last_error.as_deref(),
                    Some("API error: overloaded"),
                    "error recorded for the tail card",
                );
                assert!(stub_probe.sent().is_empty(), "push_user_message does not transmit");

                view.retry_last_turn(cx);
                assert!(view.thread.last_error.is_none(), "error cleared on retry");
                assert!(view.thread.turn_active, "retry re-opened the turn");
                assert_eq!(
                    view.thread.entries.len(),
                    1,
                    "retry re-sends the existing prompt, not a duplicate bubble",
                );
            })
            .expect("window update");

        let sent = stub_probe.sent();
        assert_eq!(sent.len(), 1, "exactly the retried prompt was transmitted");
        assert_eq!(sent[0]["message"]["content"], json!("do the thing"));
    }

    fn tool_status(view: &AgentChatView, id: &str) -> Option<&'static str> {
        view.thread.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == id => Some(match tc.status {
                ToolCallStatus::InProgress => "InProgress",
                ToolCallStatus::Rejected => "Rejected",
                ToolCallStatus::Completed => "Completed",
                ToolCallStatus::WaitingForConfirmation(_) => "WaitingForConfirmation",
                ToolCallStatus::AwaitingAnswer(_) => "AwaitingAnswer",
                _ => "Other",
            }),
            _ => None,
        })
    }

    /// An unbound *New Agent* draft switches its picked agent + model in place —
    /// rebuilding the backend transport and preselecting the new agent's default
    /// model — WITHOUT spawning a subprocess (binding waits for the first send).
    #[gpui::test]
    async fn unbound_draft_switches_agent_and_model_without_binding(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                // Drop into the draft state: Claude picked, no subprocess.
                view.make_unbound_for_test();
                assert_eq!(view.backend_transport_for_test(), Transport::StreamJson);
                assert_eq!(view.model_for_test(), Some("opus"));
                assert!(!view.is_bound_for_test(), "a draft has no connection");

                // Pick Codex: transport flips to app-server. Codex carries no
                // static pre-bind model (its real catalog arrives from the
                // `model/list` handshake), so the draft holds no model until bound
                // — and still no subprocess is spawned.
                view.change_agent("codex".into(), cx);
                assert_eq!(view.backend_transport_for_test(), Transport::AppServer);
                assert_eq!(view.model_for_test(), None);
                assert_eq!(view.unbound_agent_id_for_test(), Some("codex"));
                assert!(!view.is_bound_for_test(), "picking an agent must not bind");

                // Pick an ACP preset: transport becomes ACP; presets carry no
                // static model list, so the draft holds no model until bound.
                view.change_agent("opencode".into(), cx);
                assert_eq!(view.backend_transport_for_test(), Transport::Acp);
                assert_eq!(view.model_for_test(), None);
                assert!(!view.is_bound_for_test());

                // Back to Claude, then switch the model on the draft: it records
                // the pick (no respawn) and still hasn't bound.
                view.change_agent("claude-code".into(), cx);
                assert_eq!(view.model_for_test(), Some("opus"));
                view.change_model("sonnet".into(), cx);
                assert_eq!(view.model_for_test(), Some("sonnet"));
                assert!(!view.is_bound_for_test(), "a model pick on a draft must not bind");
            })
            .expect("window update");
    }

    /// Picking a dynamic-model agent on a test view must NOT start a live catalog
    /// probe. The probe spawns the real agent binary on a raw `std::thread`, which
    /// reaches past the injected `StubConnection` and — being owned by no executor
    /// — outlives this test; its completion then lands mid-way through a LATER
    /// test and gpui aborts the whole run for scheduler non-determinism. An empty
    /// `probed_catalogs` is the observable proof no probe was started.
    #[gpui::test]
    async fn draft_agent_pick_does_not_start_a_live_catalog_probe(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.make_unbound_for_test();
                // Codex/ACP are exactly the dynamic-model agents a live probe targets.
                view.change_agent("codex".into(), cx);
                view.change_agent("opencode".into(), cx);
                assert!(
                    view.probed_catalogs.is_empty(),
                    "a stub-connection view must not spawn a live catalog probe"
                );
            })
            .expect("window update");
    }

    /// An import bridge captions its bubbles with the provider the transcript
    /// actually came from, not the inert stream-json placeholder it assembles on
    /// — otherwise an OpenCode transcript reads as Claude's.
    ///
    /// Uses OpenCode deliberately: Pi used to be the other bridge provider, but
    /// it now opens as a live chat, so a Pi fixture here would assert a route
    /// that no longer exists.
    #[gpui::test]
    async fn import_bridge_labels_bubbles_with_its_own_provider(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::new_import_bridge(
                PathBuf::from("/tmp/oximux-bridge-label"),
                vec![ThreadEntry::Assistant(AssistantMessage {
                    text: "hi from opencode".into(),
                    thinking: String::new(),
                })],
                ImportBridge {
                    preset_id: "opencode".into(),
                    session_id: "ses-1".into(),
                    resume_handle: "ses-1".into(),
                    cwd: PathBuf::from("/tmp/oximux-bridge-label"),
                    provider_display: "OpenCode".into(),
                },
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, _cx| {
                assert!(view.is_import_bridge());
                assert_eq!(
                    view.provider_label(),
                    "OpenCode",
                    "an imported transcript must not be captioned with the placeholder backend's name"
                );
            })
            .expect("window update");
    }

    /// The companion-terminal launch spec is offered only for a bound chat that
    /// has minted a session on a resumable transport; a draft, a session-less
    /// chat, and (implicitly) an unbound draft all decline. `set_view_mode` to
    /// Terminal is a no-op until the host attaches a companion.
    #[gpui::test]
    async fn terminal_launch_spec_gates_on_bound_session(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                // Bound (stub) Claude chat, but no session id yet → no terminal,
                // and the availability reason is "no session yet".
                assert!(view.terminal_launch_spec().is_none(), "no session → no terminal");
                assert_eq!(view.terminal_availability(), TerminalAvailability::NoSessionYet);
                assert_eq!(view.view_mode(), ChatViewMode::Chat);

                // A session id makes the resume terminal available.
                view.thread.session_id = Some("sid-1".into());
                let spec = view.terminal_launch_spec().expect("bound + session → resumable");
                assert_eq!(spec.adapter_id, "claude-code");
                assert_eq!(spec.session_id, "sid-1");
                assert_eq!(view.terminal_availability(), TerminalAvailability::Available);

                // A bound ACP chat WITH a session but NO resolved preset command
                // has no interactive resume CLI wired — the toggle stays disabled,
                // but the reason is distinct from "no session yet" (the GUI-found
                // misleading-hint bug).
                view.backend.transport = Transport::Acp;
                assert!(view.terminal_launch_spec().is_none(), "ACP w/o preset → no terminal");
                assert_eq!(
                    view.terminal_availability(),
                    TerminalAvailability::NoInteractiveResume,
                    "sent a message but ACP has no resume CLI — not 'send a message first'"
                );

                // opencode: a wired preset with a confirmed interactive-resume TUI
                // → the companion terminal is offered via the Custom adapter.
                view.backend.acp_command = Some("opencode".into());
                let spec = view.terminal_launch_spec().expect("opencode → resumable");
                assert_eq!(spec.adapter, AgentAdapter::Custom);
                assert_eq!(spec.adapter_id, "opencode");
                assert_eq!(spec.session_id, "sid-1");
                assert_eq!(view.terminal_availability(), TerminalAvailability::Available);

                // amp: not confirmed → no toggle (distinct binary + unverified id).
                view.backend.acp_command = Some("amp-acp".into());
                assert!(view.terminal_launch_spec().is_none(), "amp preset unwired → no terminal");
                assert_eq!(view.terminal_availability(), TerminalAvailability::NoInteractiveResume);

                // A wired preset but an UNSAFE agent-supplied session id (leading
                // dash could be parsed as a flag) is rejected — toggle disabled.
                view.backend.acp_command = Some("opencode".into());
                view.thread.session_id = Some("-boom".into());
                assert!(view.terminal_launch_spec().is_none(), "unsafe session id → no terminal");
                view.thread.session_id = Some("sid-1".into());

                view.backend.acp_command = None;
                view.backend.transport = Transport::StreamJson;

                // Switching to Terminal is a no-op until the host attaches one.
                view.set_view_mode(ChatViewMode::Terminal, window, cx);
                assert_eq!(view.view_mode(), ChatViewMode::Chat, "no companion → stays chat");

                // An unbound draft never offers a terminal, even with a session.
                view.make_unbound_for_test();
                view.thread.session_id = Some("sid-2".into());
                assert!(view.terminal_launch_spec().is_none(), "unbound draft → no terminal");
            })
            .expect("window update");
    }

    /// The ACP session id is an external, agent-supplied string; only ids safe to
    /// place on a resume command line are accepted (the rest leave the toggle off).
    #[test]
    fn resume_session_id_charset_is_validated() {
        // Real opencode ids (alnum + `_`) and dashed ids pass.
        assert!(is_safe_resume_session_id("ses_0aea7d2e3ffeBkIyWpDmBmZ93W"));
        assert!(is_safe_resume_session_id("sid-1"));
        assert!(is_safe_resume_session_id("abc123"));
        // Empty, leading-dash (flag injection), and shell metacharacters reject.
        assert!(!is_safe_resume_session_id(""));
        assert!(!is_safe_resume_session_id("-boom"));
        assert!(!is_safe_resume_session_id("a b"));
        assert!(!is_safe_resume_session_id("a;rm -rf"));
        assert!(!is_safe_resume_session_id("$(whoami)"));
        assert!(!is_safe_resume_session_id("a/b"));
    }

    /// The signed-out banner tracks only the LATEST assistant reply (or a turn
    /// error), and offers a terminal sign-in only on a transport with an
    /// interactive login CLI.
    #[gpui::test]
    async fn signed_out_detection_tracks_latest_turn(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                // A fresh Claude chat with no auth-failure reply isn't signed out,
                // and Claude has an interactive login CLI.
                assert!(!view.is_signed_out());
                assert_eq!(view.login_adapter_id(), Some("claude-code"));

                // A "Please run /login" reply settles as an ordinary assistant
                // turn (no error) yet must trip detection.
                view.thread.entries.push(ThreadEntry::Assistant(AssistantMessage {
                    text: "Not logged in · Please run /login".into(),
                    thinking: String::new(),
                }));
                assert!(view.is_signed_out(), "login-prompt reply → signed out");

                // A later successful reply clears it — only the latest turn counts.
                view.thread.entries.push(ThreadEntry::User {
                    text: "retry".into(),
                    images: Vec::new(),
                    checkpoint: None,
                });
                view.thread.entries.push(ThreadEntry::Assistant(AssistantMessage {
                    text: "Hello! How can I help?".into(),
                    thinking: String::new(),
                }));
                assert!(!view.is_signed_out(), "a later good reply clears the banner");

                // A login-flavored turn error also trips detection.
                view.thread.last_error = Some("API Error: authentication_error".into());
                assert!(view.is_signed_out(), "auth error text → signed out");

                // ACP presets carry no bundled login CLI → no terminal sign-in.
                view.make_unbound_for_test();
                view.change_agent("opencode".into(), cx);
                assert_eq!(view.login_adapter_id(), None);
            })
            .expect("window update");
    }

    /// The *New Agent* draft's worktree control only offers itself once unbound
    /// AND for a git project — never on a bound chat, never on a non-git one.
    ///
    /// Asserted on `worktree_draft_for_composer` because that is what now carries
    /// the choice (the composer renders the pill from it). The gate itself is
    /// unchanged from when a checkbox rendered it in this view; only its owner
    /// moved.
    #[gpui::test]
    async fn worktree_control_hidden_unless_unbound_and_git(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                // Bound (the constructor's default) + git: still hidden — the
                // control is a pre-bind-only affordance.
                view.set_git_project_for_test(true);
                assert!(
                    view.worktree_draft_for_composer(cx).is_none(),
                    "bound chats never show it"
                );

                // Unbound but non-git: hidden.
                view.make_unbound_for_test();
                view.set_git_project_for_test(false);
                assert!(
                    view.worktree_draft_for_composer(cx).is_none(),
                    "non-git projects never show it"
                );

                // Unbound + git: offered.
                view.set_git_project_for_test(true);
                assert!(
                    view.worktree_draft_for_composer(cx).is_some(),
                    "unbound + git offers the control"
                );

                // The status banner is a different thing: it carries only the
                // in-flight/failure state, so at rest it stays out of the way
                // rather than reserving an empty strip above the composer.
                assert!(
                    view.render_worktree_status_banner(cx).is_none(),
                    "no banner while the create state is Idle"
                );
            })
            .expect("window update");
    }

    /// The pill emits the DESIRED isolation, not a flip, so re-picking the row
    /// that is already active must be a no-op rather than silently toggling the
    /// choice to the opposite of what the user clicked.
    #[gpui::test]
    async fn worktree_isolation_pick_is_idempotent_and_reaches_the_draft(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                assert!(!view.worktree_draft_enabled_for_test());

                // Pick "New worktree" → armed, and the slug field materializes.
                view.set_worktree_isolation(true, window, cx);
                assert!(view.worktree_draft_enabled_for_test());
                let draft = view.worktree_draft_for_composer(cx).expect("offered");
                assert!(draft.enabled);
                assert!(draft.slug_input.is_some(), "arming creates the slug field");
                assert!(draft.hint.starts_with("oximux/"), "hint previews the branch: {}", draft.hint);

                // Re-picking the SAME row must not flip it back off.
                view.set_worktree_isolation(true, window, cx);
                assert!(
                    view.worktree_draft_enabled_for_test(),
                    "re-picking the active row is a no-op, not a toggle"
                );

                // Picking the other row disarms.
                view.set_worktree_isolation(false, window, cx);
                assert!(!view.worktree_draft_enabled_for_test());
            })
            .expect("window update");
    }

    /// `/clear` on a never-bound *New Agent* draft must do nothing — above all it
    /// must not spawn.
    ///
    /// A draft is already a fresh conversation: `bind_now` drops `unbound` before
    /// the first message can land, so an empty transcript is an invariant here.
    /// `new_chat` used to respawn unconditionally, which spawned a subprocess
    /// while `unbound` stayed true — a live connection the view still treated as
    /// a draft, so the composer kept offering the pre-bind agent picker and
    /// static model list for a session already advertising real capabilities.
    ///
    /// Catching a stray spawn takes BOTH assertions below, because `respawn` can
    /// fail as well as succeed and the two leave different traces:
    /// - it succeeded → `connection` is `Some`
    /// - it failed → `disconnected` + `last_error` are set (`respawn_with_env`'s
    ///   `Err` arm)
    ///
    /// Only the second bites in a unit test, where `connect()` cannot succeed (no
    /// Tokio runtime). Asserting on `connection` alone looks right and proves
    /// nothing — it stays `None` whether or not the guard exists.
    #[gpui::test]
    async fn clear_on_an_unbound_draft_does_not_spawn_or_bind(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.make_unbound_for_test();
                view.sync_unbound_composer(cx);
                assert!(
                    !view.connection_is_live_for_test(),
                    "precondition: a draft has no subprocess"
                );

                view.send_text("/clear".into(), Vec::new(), cx);

                assert!(
                    !view.connection_is_live_for_test(),
                    "/clear on a draft must not spawn — the transport is choosable \
                     until the first real send"
                );
                // The one that actually bites here: a *failed* spawn attempt is
                // still an attempt, and it leaves these behind.
                assert!(
                    !view.disconnected,
                    "/clear must not even attempt a spawn — a draft is already fresh"
                );
                assert!(
                    view.thread.last_error.is_none(),
                    "a draft that was never sent to cannot have failed to resume: {:?}",
                    view.thread.last_error
                );
                assert!(view.is_unbound(), "/clear must not bind the draft");
                assert!(view.thread.entries.is_empty(), "a draft has nothing to clear");
                // The draft's composer shape is untouched: the user can still pick
                // an agent after typing /clear.
                assert!(
                    view.composer.read(cx).unbound_for_test(),
                    "the draft keeps its pre-bind picker shape"
                );
            })
            .expect("window update");
    }

    // NOTE: `/clear` on a BOUND chat is deliberately not unit-tested. `new_chat`
    // reaches `respawn` → `connect()`, which starts a real subprocess — a unit
    // test must not do that (the same hazard that made the catalog probe SIGABRT
    // the suite). Covering it needs a spawn seam on `respawn`, which every other
    // respawn path shares (Stop-resume, model switch, auth, rewind), so that is a
    // design change on its own merits rather than a rider on this fix. Splitting
    // the transcript-reset into a helper and asserting on that instead would only
    // prove the helper works, not that `new_chat` still calls it.

    /// Binding must clear the worktree pill from the composer: a live session's
    /// cwd is fixed, so offering to change it is a lie.
    ///
    /// This is the mirror of `sync_while_unbound_keeps_the_draft_picker_shape`.
    /// The pill is pushed from `sync_unbound_composer`, which stops running once
    /// bound — so the *bound* sync has to clear it explicitly, exactly as it
    /// already clears the agent picker. Caught live (the dimmed pill lingered
    /// beside a bound chat's real controls), not by the unit tests above: none of
    /// them bind, which is precisely the gap this closes.
    #[gpui::test]
    async fn binding_clears_the_worktree_pill(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.set_worktree_isolation(true, window, cx);
                assert!(
                    view.composer.read(cx).worktree_draft_is_some_for_test(),
                    "precondition: an armed draft shows the pill"
                );

                // Bind, as a successful worktree create + send would.
                view.make_bound_for_test();
                view.sync_composer(cx);

                assert!(
                    !view.composer.read(cx).worktree_draft_is_some_for_test(),
                    "a bound chat's cwd is fixed — the pill must not linger"
                );
            })
            .expect("window update");
    }

    /// The pill must carry the parent's refusal to change the pick while a create
    /// is in flight / has failed with a message staged — otherwise it would
    /// render enabled and swallow clicks, which reads as a broken control.
    #[gpui::test]
    async fn worktree_draft_reports_busy_while_create_is_not_idle(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.set_worktree_isolation(true, window, cx);
                assert!(!view.worktree_draft_for_composer(cx).expect("offered").busy);

                view.send_text("hello".into(), Vec::new(), cx);

                let draft = view.worktree_draft_for_composer(cx).expect("offered");
                assert!(draft.busy, "an in-flight create freezes the pick");
                // And the underlying rule still holds: the pick cannot change.
                view.set_worktree_isolation(false, window, cx);
                assert!(
                    view.worktree_draft_enabled_for_test(),
                    "the pick must not change while a message is staged"
                );
            })
            .expect("window update");
    }

    /// Toggling on creates the lazily-built slug `InputState` (and the toggle
    /// keeps rendering with it); toggling back off drops it and resets any
    /// stale create-state — mirroring `reconcile_env_inputs`'s create-on-demand
    /// pattern for the EnvVar-auth fields.
    #[gpui::test]
    async fn toggling_worktree_draft_creates_and_drops_the_slug_input(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                assert!(!view.worktree_draft_enabled_for_test());

                view.toggle_worktree_draft(window, cx);
                assert!(view.worktree_draft_enabled_for_test(), "first toggle enables it");
                assert!(view.worktree_slug_input.is_some(), "slug input created on enable");

                view.toggle_worktree_draft(window, cx);
                assert!(!view.worktree_draft_enabled_for_test(), "second toggle disables it");
                assert!(view.worktree_slug_input.is_none(), "slug input dropped on disable");
            })
            .expect("window update");
    }

    /// The first send on an armed draft is gated on the (async) worktree
    /// create landing first — `start_worktree_then_send` validates the slug,
    /// marks the create in-flight (`Creating`), stages the message, and emits
    /// `WorktreeWorkspaceRequested` for the host to run the DB-backed create.
    /// It does NOT bind/spawn or push the message to the transcript yet. With no
    /// host subscriber wired in this unit harness the request is inert, so the
    /// state parks at `Creating` — letting this assert the staging invariants
    /// (nothing bound, nothing in the transcript) without any git/process side
    /// effects.
    #[gpui::test]
    async fn send_on_armed_draft_stages_the_message_instead_of_binding(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.toggle_worktree_draft(window, cx);
                assert!(view.worktree_draft_enabled_for_test());

                view.send_text("hello".into(), Vec::new(), cx);

                // The roster stages the send and hands the create to the host —
                // the state is in-flight (`Creating`), never bound, transcript
                // still empty.
                assert!(
                    matches!(
                        view.worktree_create_state_for_test(),
                        roster::WorktreeCreateState::Creating
                    ),
                    "armed send marks the worktree create in-flight"
                );
                assert!(!view.is_bound_for_test(), "must not bind before the worktree step lands");
                assert!(
                    view.thread.entries.is_empty(),
                    "the message stays staged, never pushed to the transcript"
                );

                // Retry re-enters the same path with the same staged text —
                // still in-flight, still nothing pushed.
                view.retry_worktree_create(cx);
                assert!(matches!(
                    view.worktree_create_state_for_test(),
                    roster::WorktreeCreateState::Creating
                ));
                assert!(view.thread.entries.is_empty());
            })
            .expect("window update");
    }

    /// Regression: syncing the composer while the draft is still UNBOUND must
    /// not push the bound-chat shape into it. The composer keeps its own
    /// `unbound` flag, and the agent picker, the Import-session row and the
    /// placeholder's agent name all read that one — so a `sync_composer` that
    /// unconditionally cleared it stripped all three from a live New Agent
    /// draft, with no way to restore them.
    ///
    /// The worktree toggle is the trigger that made this reachable (it syncs the
    /// composer to reflect its own busy state), but the bug is in the sync, not
    /// the toggle: every one of `sync_composer`'s ~23 callers could hit it while
    /// unbound. Asserted through the toggle because that is the path a user
    /// actually walks.
    #[gpui::test]
    async fn sync_while_unbound_keeps_the_draft_picker_shape(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                // Seed the draft's picker shape the way the real constructor does.
                view.sync_unbound_composer(cx);
                assert!(
                    view.composer.read(cx).unbound_for_test(),
                    "precondition: the draft seeds the composer as unbound"
                );
                let seeded_agents = view.composer.read(cx).agent_options_len_for_test();
                assert!(seeded_agents > 0, "precondition: the draft offers agents to pick");
                let seeded_models = view.composer.read(cx).vocab_models_len_for_test();
                assert!(seeded_models > 0, "precondition: the draft offers models to pick");

                // Flipping the worktree toggle syncs the composer. Before the fix
                // this reached `set_agent_picker(false, vec![], None)` and blanked
                // the draft.
                view.toggle_worktree_draft(window, cx);

                assert!(
                    view.composer.read(cx).unbound_for_test(),
                    "the toggle must not bind the composer — the Import row and the \
                     placeholder's agent name are gated on this flag"
                );
                assert_eq!(
                    view.composer.read(cx).agent_options_len_for_test(),
                    seeded_agents,
                    "the agent picker must survive an unbound sync"
                );
                // Asserted separately: the model picker reads `vocab.models`, not
                // `unbound`, so the two assertions above would both hold while the
                // model list was blanked on its own.
                assert_eq!(
                    view.composer.read(cx).vocab_models_len_for_test(),
                    seeded_models,
                    "the model picker must survive an unbound sync — a draft has no \
                     connection, so the caps-derived vocab is empty"
                );
            })
            .expect("window update");
    }

    /// HIGH regression: a SECOND, distinct Submit while a worktree create is
    /// already in flight (or one failed with a message still staged) must
    /// NEVER fall through `send_text`'s `bind_now` — that would bind at the
    /// ORIGINAL cwd (silently defeating the toggle), and once the in-flight
    /// create landed, the FIRST staged message would be re-sent into that
    /// now-wrongly-bound session: duplicated/out-of-order sends plus an
    /// orphaned worktree. `worktree_create_state` is set to `Creating`
    /// directly (mirroring what `start_worktree_then_send` does before the
    /// git op resolves) so this is exercised without a real async race.
    #[gpui::test]
    async fn second_submit_during_worktree_creating_does_not_bind_or_duplicate(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.toggle_worktree_draft(window, cx);
                assert!(view.worktree_draft_enabled_for_test());

                // Simulate the async create landing mid-flight: the first
                // message is already staged and the create is running.
                view.worktree_create_state = roster::WorktreeCreateState::Creating;
                view.pending_worktree_send = Some(("first message".to_string(), Vec::new()));
                view.sync_composer(cx);

                // A second, distinct Submit arrives (e.g. a stray dispatch —
                // the composer itself is already disabled for this via
                // `sync_composer`'s fold into `disconnected`, so this is the
                // defense-in-depth path `send_text` itself must also close).
                view.send_text("second message".into(), Vec::new(), cx);

                assert!(
                    !view.is_bound_for_test(),
                    "must not bind at the original cwd while the worktree create is in flight"
                );
                assert!(
                    view.thread.entries.is_empty(),
                    "no message should reach the transcript until the worktree step lands"
                );
                assert_eq!(
                    view.pending_worktree_send.as_ref().map(|(t, _)| t.as_str()),
                    Some("first message"),
                    "the original staged message must survive untouched, not be clobbered"
                );
                assert!(
                    matches!(view.worktree_create_state_for_test(), roster::WorktreeCreateState::Creating),
                    "state is unchanged by the dropped second submit"
                );
            })
            .expect("window update");
    }

    /// MEDIUM regression: the toggle checkbox must refuse to flip while a
    /// worktree create has failed with a message still staged — otherwise
    /// unchecking it silently discards `pending_worktree_send`. The user must
    /// go through Retry / "continue without a worktree" instead, both of
    /// which route the staged message onward.
    #[gpui::test]
    async fn toggle_is_inert_while_worktree_create_failed_so_the_staged_message_survives(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.toggle_worktree_draft(window, cx);
                assert!(view.worktree_draft_enabled_for_test());

                // Simulate a landed failure with the message still staged.
                view.worktree_create_state =
                    roster::WorktreeCreateState::Failed("slug already exists".to_string());
                view.pending_worktree_send = Some(("keep me".to_string(), Vec::new()));
                view.sync_composer(cx);

                // A click on the checkbox while Failed must be a no-op.
                view.toggle_worktree_draft(window, cx);

                assert!(
                    view.worktree_draft_enabled_for_test(),
                    "the toggle must stay on — it must not flip while Failed"
                );
                assert!(
                    matches!(
                        view.worktree_create_state_for_test(),
                        roster::WorktreeCreateState::Failed(_)
                    ),
                    "the failed state is untouched by the ignored toggle"
                );
                assert_eq!(
                    view.pending_worktree_send.as_ref().map(|(t, _)| t.as_str()),
                    Some("keep me"),
                    "the staged message must survive the ignored toggle"
                );
                // The only sanctioned way out is Retry / `send_without_worktree`
                // (the failure banner's buttons) — not exercised here since
                // both ultimately reach `bind_now`'s real subprocess spawn,
                // which this pure state-transition test intentionally avoids
                // (mirrors every other `make_unbound_for_test` test in this
                // file never calling into a real connect()).
            })
            .expect("window update");
    }
