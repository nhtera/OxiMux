//! Composer tests, lifted out of `composer.rs` so that file can ratchet back
//! under the size cap. A pure move of the `#[cfg(test)]` items — the
//! test-only `impl ComposerView` helpers and the three test modules —
//! with no change to any of them.

use super::*;

#[cfg(test)]
impl ComposerView {
    /// The composer's OWN `unbound` flag — distinct from the parent view's, and
    /// pushed down via [`Self::set_agent_picker`]. The agent picker, the
    /// Import-session row and the placeholder's agent name all read this one, so
    /// a test asserting they survive a parent-side sync must read it here rather
    /// than the parent's.
    pub(crate) fn unbound_for_test(&self) -> bool {
        self.unbound
    }

    /// How many agents the picker currently offers — cleared to zero by a
    /// bound-chat sync, which is the other half of the same push.
    pub(crate) fn agent_options_len_for_test(&self) -> usize {
        self.agent_options.len()
    }

    /// Whether the worktree pill is currently offered. Pushed by the parent, and
    /// cleared by a bound sync — a live session's cwd can't change.
    pub(crate) fn worktree_draft_is_some_for_test(&self) -> bool {
        self.worktree_draft.is_some()
    }

    /// How many models the picker currently offers. Gated independently of
    /// `unbound` (the picker reads `!vocab.models.is_empty()`), so a test
    /// asserting only the two above would miss a regression that blanks the
    /// model list alone — which is exactly what pushing a connection-less
    /// draft's empty caps-derived vocab does.
    pub(crate) fn vocab_models_len_for_test(&self) -> usize {
        self.vocab.models.len()
    }

    /// Set the draft text so a `#[gpui::test]` can exercise submit / newline /
    /// palette routing without synthesising keystrokes. Parks the caret at the
    /// end (as if the text were typed) so trigger detection sees the token.
    pub(crate) fn set_draft_for_test(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_draft_end(text.to_string(), window, cx);
    }

    /// Read the current draft (to assert it survived a newline or was cleared by
    /// submit).
    pub(crate) fn draft_for_test(&self, cx: &Context<Self>) -> String {
        self.input.read(cx).value().to_string()
    }

    /// The caret's byte offset — to assert an accepted command parks it after the
    /// inserted token rather than jumping to the start of the box.
    pub(crate) fn cursor_for_test(&self, cx: &Context<Self>) -> usize {
        self.input.read(cx).cursor()
    }

    /// Recompute the overlays from the current draft (as an edit's `Change` would)
    /// so a test can open the palette before accepting a command.
    pub(crate) fn recompute_overlays_for_test(&mut self, cx: &mut Context<Self>) {
        self.recompute_overlays(cx);
    }

    /// Accept the highlighted palette command (as Tab/Enter would).
    pub(crate) fn accept_highlighted_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.palette_accept_highlighted(window, cx)
    }

    /// The `(command, argument-hint)` the usage strip would show, or `None`.
    pub(crate) fn usage_hint_for_test(&self, cx: &Context<Self>) -> Option<(String, String)> {
        self.usage_hint(cx)
    }

    /// Drive ↑ history recall (as the MoveUp capture would); returns whether it
    /// consumed the key.
    pub(crate) fn history_older_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.history_older(window, cx)
    }

    /// Drive ↓ history recall (as the MoveDown capture would).
    pub(crate) fn history_newer_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.history_newer(window, cx)
    }

    /// How many messages are currently parked (queued while a turn streamed).
    pub(crate) fn queued_len_for_test(&self) -> usize {
        self.queued.len()
    }

    /// Drive ↑ "edit the last queued message"; returns whether it consumed the key.
    pub(crate) fn edit_last_queued_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.edit_last_queued(window, cx)
    }

    /// Stage a context chip directly (as the parent's capture would), so a test
    /// can exercise serialization / clear-on-send without a live provider.
    pub(crate) fn stage_context_chip_for_test(&mut self, chip: ContextChip, cx: &mut Context<Self>) {
        self.add_context_chip(chip, cx);
    }

    /// How many context chips are currently staged.
    pub(crate) fn context_chips_len_for_test(&self) -> usize {
        self.context_chips.len()
    }

    /// The palette enrichment currently in effect (descriptions + grouping).
    pub(crate) fn slash_catalog_for_test(&self) -> &CommandCatalog {
        &self.slash_catalog
    }

    /// Stage an attachment directly (as the picker/paste path would), so a test
    /// can exercise the with-images branches without decoding a real image.
    pub(crate) fn stage_image_for_test(&mut self, chat: ChatImage) {
        let render = image_attach::decode_render(&chat);
        self.pending_images.push(PendingImage {
            chat,
            render: render.expect("a test image must decode"),
        });
    }
}

#[test]
fn a_mention_never_fuses_onto_the_last_typed_word() {
    // The failure this guards: dropping a file after "fix this" producing
    // `fix this@src/main.rs`, which is neither a mention nor prose.
    assert_eq!(
        with_mentions_appended("fix this", &["src/main.rs".into()]),
        "fix this @src/main.rs "
    );
    // A draft that already ends in whitespace gets no second space.
    assert_eq!(
        with_mentions_appended("fix this ", &["src/main.rs".into()]),
        "fix this @src/main.rs "
    );
    assert_eq!(
        with_mentions_appended("fix this\n", &["src/main.rs".into()]),
        "fix this\n@src/main.rs "
    );
}

#[test]
fn an_empty_draft_takes_no_leading_space() {
    assert_eq!(with_mentions_appended("", &["a.rs".into()]), "@a.rs ");
}

#[test]
fn several_dropped_files_stay_separated() {
    // A multi-file drop is one gesture; the tokens must not run together.
    assert_eq!(
        with_mentions_appended("", &["a.rs".into(), "b/c.rs".into()]),
        "@a.rs @b/c.rs "
    );
}

#[test]
fn no_paths_leaves_the_draft_untouched() {
    assert_eq!(with_mentions_appended("keep me", &[]), "keep me");
}

use gpui::TestAppContext;

#[test]
fn short_model_label_strips_provider_namespace() {
    // The toolbar trigger drops any `provider/` prefix; the full label is kept
    // for the dropdown row + search.
    assert_eq!(short_model_label("openai/gpt-5.5"), "gpt-5.5");
    assert_eq!(short_model_label("opencode/big-pickle"), "big-pickle");
    // No namespace → unchanged; only the last segment survives nesting.
    assert_eq!(short_model_label("Sonnet"), "Sonnet");
    assert_eq!(short_model_label("a/b/c"), "c");
}

#[test]
fn model_item_search_matches_name_or_description() {
    let opus = ModelItem {
        wire: "opus".into(),
        label: "Opus".into(),
        description: Some("Most capable — deep reasoning & hard tasks".into()),
    };
    // Name match (case-insensitive).
    assert!(opus.matches("opus"));
    assert!(opus.matches("OP"));
    // Description match: a capability query finds it even though "reasoning"
    // isn't in the name.
    assert!(opus.matches("reasoning"));
    assert!(opus.matches("capable"));
    // Neither → no match.
    assert!(!opus.matches("haiku"));

    // A model without a blurb only matches on its name.
    let bare = ModelItem { wire: "o3".into(), label: "o3".into(), description: None };
    assert!(bare.matches("o3"));
    assert!(!bare.matches("reasoning"));
}

/// Build a bare composer in a test window (no parent needed).
fn test_composer(cx: &mut TestAppContext) -> gpui::WindowHandle<ComposerView> {
    cx.update(gpui_component::init);
    let w = cx.add_window(|window, cx| {
        ComposerView::new(
            Theme::default(),
            Density::default(),
            Typography::default(),
            "Claude",
            window,
            cx,
        )
    });
    cx.run_until_parked();
    w
}

/// ↑ recalls previously-sent prompts newest-first and stops at the oldest;
/// ↓ walks back forward and restores the live draft past the newest.
#[gpui::test]
async fn up_down_arrow_recall_prompt_history(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            c.seed_history(vec!["alpha".into(), "beta".into()]);
            // Empty draft, caret on the first line → ↑ recalls the newest.
            assert!(c.history_older_for_test(window, cx));
            assert_eq!(c.draft_for_test(cx), "beta");
            assert!(c.history_older_for_test(window, cx));
            assert_eq!(c.draft_for_test(cx), "alpha");
            // At the oldest: stays put but still consumes the key.
            assert!(c.history_older_for_test(window, cx));
            assert_eq!(c.draft_for_test(cx), "alpha");
            // ↓ walks forward, then restores the (empty) live draft and stops
            // consuming once out of history.
            assert!(c.history_newer_for_test(window, cx));
            assert_eq!(c.draft_for_test(cx), "beta");
            assert!(c.history_newer_for_test(window, cx));
            assert_eq!(c.draft_for_test(cx), "");
            assert!(!c.history_newer_for_test(window, cx), "not navigating → fall through");
        })
        .expect("window update");
}

/// With no history, ↑ is not consumed (so the caret can still move).
#[gpui::test]
async fn up_arrow_falls_through_with_empty_history(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            assert!(!c.history_older_for_test(window, cx));
        })
        .expect("window update");
}

/// Submitting while a turn streams parks the message (the queue branch in
/// `submit` returns before it can emit a Submit) and clears the draft; the
/// parked message is then handed back on drain, emptying the queue.
#[gpui::test]
async fn submit_during_turn_queues_instead_of_sending(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            // Simulate a streaming turn.
            c.set_state(false, true, cx);
            c.set_draft_for_test("queued one", window, cx);
            c.submit(window, cx);
            assert_eq!(c.queued_len_for_test(), 1, "message parked, not sent");
            assert!(c.draft_for_test(cx).is_empty(), "draft cleared on queue");
            // Drain hands the parked message back and empties the queue.
            let next = c.take_next_queued(cx);
            assert_eq!(next.map(|(t, _)| t), Some("queued one".to_string()));
            assert_eq!(c.queued_len_for_test(), 0);
        })
        .expect("window update");
}

/// "Send now" while a turn streams moves a queued message to the FRONT (next
/// to auto-drain) without sending; it's a no-op at index 0.
#[gpui::test]
async fn send_queued_now_moves_to_front_mid_turn(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            c.set_state(false, true, cx); // streaming turn
            for t in ["one", "two", "three"] {
                c.set_draft_for_test(t, window, cx);
                c.submit(window, cx);
            }
            assert_eq!(c.queued_texts(), vec!["one", "two", "three"]);
            // Jump the third to the front.
            c.send_queued_now(2, cx);
            assert_eq!(c.queued_texts(), vec!["three", "one", "two"], "moved to front, none sent");
            // No-op at index 0.
            c.send_queued_now(0, cx);
            assert_eq!(c.queued_texts(), vec!["three", "one", "two"], "index 0 is a no-op");
        })
        .expect("window update");
}

/// On a backend that takes a mid-turn message, "send now" means now: the chip
/// leaves the queue for the running turn instead of shuffling within it.
#[gpui::test]
async fn send_queued_now_steers_mid_turn_when_the_backend_takes_it(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            c.set_state(false, true, cx); // streaming turn
            c.set_can_steer(true, cx);
            for t in ["one", "two"] {
                c.set_draft_for_test(t, window, cx);
                c.submit(window, cx);
            }
            // Even at index 0 — the case that is a no-op without steering —
            // the message goes out rather than being re-parked at the front.
            c.send_queued_now(0, cx);
            assert_eq!(c.queued_texts(), vec!["two"], "the steered message left the queue");
            // It counts as sent, so ↑ recall offers it.
            c.send_queued_now(0, cx);
            assert_eq!(c.queued_texts(), Vec::<String>::new());
        })
        .expect("window update");
}

/// A queued message carrying an image never steers — pi's `steer` accepts
/// images but OxiMux has never sent one, and reordering keeps the attachment
/// rather than quietly dropping it.
#[gpui::test]
async fn a_queued_message_with_an_image_reorders_instead_of_steering(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            c.set_state(false, true, cx);
            c.set_can_steer(true, cx);
            c.set_draft_for_test("text only", window, cx);
            c.submit(window, cx);
            c.stage_image_for_test(ChatImage {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            });
            c.set_draft_for_test("has an image", window, cx);
            c.submit(window, cx);
            c.send_queued_now(1, cx);
            assert_eq!(
                c.queued_texts(),
                vec!["has an image", "text only"],
                "moved to the front, still queued — its image is intact"
            );
        })
        .expect("window update");
}

/// End-to-end for Design Mode: an element picked in the embedded browser is
/// staged exactly as `AgentChatView::stage_browser_pick` stages it, then sent.
///
/// This is the seam a unit test cannot reach and a webview is not needed for:
/// it proves the capture survives chip staging, image decode, and submit, and
/// that both halves reach the wire together.
#[gpui::test]
async fn a_picked_browser_element_reaches_the_wire_with_its_screenshot(
    cx: &mut TestAppContext,
) {
    // A 1x1 red PNG — the smallest thing `pending_from_bytes` will accept.
    use base64::Engine as _;
    let png: Vec<u8> = base64::engine::general_purpose::STANDARD
        .decode(concat!(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8",
            "z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
        ))
        .expect("valid base64 fixture");

    let window = test_composer(cx);
    let sent = std::rc::Rc::new(std::cell::RefCell::new(None));
    let seen = sent.clone();
    cx.update(|cx| {
        let root = window.root(cx).expect("root view");
        cx.subscribe(
            &root,
            move |_, ev: &ComposerEvent, _| {
                if let ComposerEvent::Submit { text, images } = ev {
                    *seen.borrow_mut() = Some((text.clone(), images.clone()));
                }
            },
        )
        .detach();
    });

    window
        .update(cx, |c, window, cx| {
            // Exactly what `stage_browser_pick` does with a pick.
            let chip = crate::shell::agent_chat::context_providers::browser_chip(
                "a#go",
                "Selected element: <a id=\"go\">x</a>\ncolor: rgb(0, 0, 0)".to_string(),
            )
            .expect("a non-empty capture makes a chip");
            assert!(chip.label().starts_with("@browser a#go · "));
            c.add_context_chip(chip, cx);
            let staged = image_attach::pending_from_bytes(png, None)
                .expect("a 1x1 PNG decodes");
            c.add_pending_images(vec![staged], cx);

            // The capture is staged, not sent — the user still types the ask.
            assert_eq!(c.context_chips_len_for_test(), 1);
            assert_eq!(c.current_images().len(), 1);

            c.set_draft_for_test("why is this misaligned?", window, cx);
            c.submit(window, cx);
        })
        .expect("window update");
    cx.run_until_parked();

    let (text, images) = sent.borrow_mut().take().expect("a submit was emitted");
    // The element rides as a tagged context block naming its selector...
    assert!(
        text.starts_with("<context name=\"browser\" source=\"a#go\">\n"),
        "wire text was: {text}"
    );
    assert!(text.contains("<a id=\"go\">x</a>"), "the HTML must survive");
    assert!(text.contains("color: rgb(0, 0, 0)"), "the computed CSS must survive");
    // ...and the user's own question is still the tail of the message.
    assert!(text.ends_with("why is this misaligned?"));
    // ...with the crop attached, not left on the clipboard.
    assert_eq!(images.len(), 1, "the screenshot must travel with the text");
    assert_eq!(images[0].media_type, "image/png");
    assert!(!images[0].data.is_empty());
}

/// Restored queued chips (`seed_queued`) re-render without auto-sending; a
/// seeded draft respects the no-clobber guard.
#[gpui::test]
async fn seed_queue_and_draft_restore_without_clobber(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            // Restored queue: chips appear, nothing sent (no active turn).
            c.seed_queued(vec!["a".into(), "b".into(), "  ".into()], cx);
            assert_eq!(c.queued_texts(), vec!["a", "b"], "blank entry skipped, none sent");
            // seed_draft into an empty composer applies.
            c.seed_draft("restored draft".into(), window, cx);
            assert_eq!(c.draft_for_test(cx), "restored draft");
            // seed_draft into a NON-empty composer is ignored (no clobber).
            c.seed_draft("should be ignored".into(), window, cx);
            assert_eq!(c.draft_for_test(cx), "restored draft", "in-progress text preserved");
        })
        .expect("window update");
}

/// ↑ with an empty draft while a message is parked pulls it back into the
/// composer to edit (removing it from the queue); with a non-empty draft it
/// does not clobber the in-progress text.
#[gpui::test]
async fn up_arrow_edits_the_last_queued_message(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            c.set_state(false, true, cx); // streaming turn
            c.set_draft_for_test("park me", window, cx);
            c.submit(window, cx);
            assert_eq!(c.queued_len_for_test(), 1);
            assert!(c.draft_for_test(cx).is_empty());
            // Empty draft + a queued message → ↑ pulls it back for editing.
            assert!(c.edit_last_queued_for_test(window, cx));
            assert_eq!(c.draft_for_test(cx), "park me");
            assert_eq!(c.queued_len_for_test(), 0, "pulled out of the queue");
            // With a draft present now, ↑ must not pull (nothing queued anyway).
            assert!(!c.edit_last_queued_for_test(window, cx));
        })
        .expect("window update");
}

/// A staged context chip serializes into the drained message as a `<context>`
/// block prepended to the typed text — and a chip alone (no caption) is enough
/// to send (the empty-guard allows an attachment-only prompt).
#[gpui::test]
async fn context_chip_serializes_into_wire_on_drain(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            c.set_state(false, true, cx); // streaming → submit parks it
            c.stage_context_chip_for_test(
                ContextChip::new(
                    oximux_agents::thread::ContextKind::Diff,
                    None,
                    "diff --git a b".into(),
                    false,
                ),
                cx,
            );
            c.set_draft_for_test("what changed?", window, cx);
            c.submit(window, cx);
            assert_eq!(c.queued_len_for_test(), 1, "parked with its chip");
            assert_eq!(c.context_chips_len_for_test(), 0, "chip drained off staging");
            let (wire, _) = c.take_next_queued(cx).expect("one queued");
            assert!(wire.starts_with("<context name=\"diff\">"), "block prepended: {wire}");
            assert!(wire.ends_with("what changed?"), "typed text preserved: {wire}");
        })
        .expect("window update");
}

/// Pulling a queued message back to edit restores its context chips (they
/// re-serialize on the next send), mirroring image restore.
#[gpui::test]
async fn queued_context_chip_restored_on_edit(cx: &mut TestAppContext) {
    let window = test_composer(cx);
    window
        .update(cx, |c, window, cx| {
            c.set_state(false, true, cx);
            c.stage_context_chip_for_test(
                ContextChip::new(
                    oximux_agents::thread::ContextKind::Clipboard,
                    None,
                    "pasted".into(),
                    false,
                ),
                cx,
            );
            c.submit(window, cx);
            assert_eq!(c.queued_len_for_test(), 1);
            assert_eq!(c.context_chips_len_for_test(), 0);
            assert!(c.edit_last_queued_for_test(window, cx));
            assert_eq!(c.context_chips_len_for_test(), 1, "chip restored on edit");
            assert_eq!(c.queued_len_for_test(), 0);
        })
        .expect("window update");
}

#[test]
fn rank_context_sources_prefix_then_substring_then_all() {
    let sources = vec![
        ContextSource::diff(),                                    // key "diff"
        ContextSource::clipboard(),                              // key "clipboard"
        ContextSource::terminal(oximux_pty::TerminalSessionId(1), "diffbuild"), // key "terminal diffbuild"
    ];
    // Empty query → all sources in order.
    assert_eq!(rank_context_sources(&sources, ""), vec![0, 1, 2]);
    // "diff" prefixes source 0, is a substring of source 2 → prefix first.
    assert_eq!(rank_context_sources(&sources, "diff"), vec![0, 2]);
    // "clip" prefixes only clipboard.
    assert_eq!(rank_context_sources(&sources, "clip"), vec![1]);
    // No match.
    assert!(rank_context_sources(&sources, "zzz").is_empty());
}
#[cfg(test)]
mod agent_count_tests {
    use super::AgentModelCount;

    #[test]
    fn count_labels() {
        assert_eq!(AgentModelCount::Known(4).label(), Some(("· 4 models".into(), false)));
        assert_eq!(AgentModelCount::Known(1).label(), Some(("· 1 model".into(), false)));
        assert_eq!(AgentModelCount::Loading.label(), Some(("…".into(), false)));
        assert_eq!(AgentModelCount::Failed.label(), Some(("Error".into(), true)));
        assert_eq!(AgentModelCount::Unknown.label(), None);
    }
}
#[cfg(test)]
mod display_wire_tests {
    use super::*;

    fn catalog() -> Vec<ModelChoice> {
        ["opus[1m]", "claude-fable-5[1m]", "claude-fable-5-1[1m]", "sonnet", "haiku"]
            .into_iter()
            .map(|w| ModelChoice { wire: w.to_string(), label: w.to_string(), description: None })
            .collect()
    }

    #[test]
    fn a_catalog_wire_is_itself() {
        assert_eq!(resolve_display_wire("sonnet", &catalog()).as_deref(), Some("sonnet"));
        assert_eq!(resolve_display_wire("opus[1m]", &catalog()).as_deref(), Some("opus[1m]"));
    }

    #[test]
    fn a_legacy_alias_maps_to_its_family_row() {
        assert_eq!(resolve_display_wire("opus", &catalog()).as_deref(), Some("opus[1m]"));
        // Two Fable rows: the first listed wins, which is display-only anyway.
        assert_eq!(
            resolve_display_wire("fable", &catalog()).as_deref(),
            Some("claude-fable-5[1m]")
        );
        assert_eq!(
            resolve_display_wire("claude-opus-4-1", &catalog()).as_deref(),
            Some("opus[1m]")
        );
    }

    #[test]
    fn an_unrelated_wire_selects_nothing() {
        assert_eq!(resolve_display_wire("gpt", &catalog()), None);
        assert_eq!(resolve_display_wire("", &catalog()), None);
        assert_eq!(resolve_display_wire("opus", &[]), None);
        // A non-Claude catalog: a wire that left it must not pick a sibling.
        let codex: Vec<ModelChoice> = ["gpt-5.5", "gpt-5.5-codex"]
            .into_iter()
            .map(|w| ModelChoice { wire: w.to_string(), label: w.to_string(), description: None })
            .collect();
        assert_eq!(resolve_display_wire("gpt-5.4", &codex), None);
        assert_eq!(resolve_display_wire("gpt-5.5", &codex).as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn family_token_shapes() {
        assert_eq!(model_family("opus[1m]"), Some("opus"));
        assert_eq!(model_family("claude-fable-5-1[1m]"), Some("fable"));
        assert_eq!(model_family("claude-sonnet-5"), Some("sonnet"));
        assert_eq!(model_family("haiku"), Some("haiku"));
        assert_eq!(model_family("gpt-5.4"), None);
    }
}
