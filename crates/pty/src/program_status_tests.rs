use super::*;

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

fn set(payload: &str) -> (Vec<String>, ProgramRecord) {
    match parse_report(payload.as_bytes()) {
        Some(ProgramReport::Set { id, record }) => (id, record),
        other => panic!("expected a set report for {payload:?}, got {other:?}"),
    }
}

fn path(id: &str) -> Vec<String> {
    id.split('/').map(str::to_owned).collect()
}

#[test]
fn query_is_recognized_and_never_parsed_as_a_report() {
    assert!(is_query(b"?"));
    assert!(is_query(b"?:states=idle"));
    assert!(!is_query(b"state=idle"));
    assert_eq!(parse_report(b"?"), None);
}

#[test]
fn the_reports_a_real_agent_sends_parse() {
    let (id, r) = set("state=idle:app=claude-code");
    assert!(id.is_empty());
    assert_eq!(r.state, ProgramState::Idle);
    assert_eq!(r.app.as_deref(), Some("claude-code"));

    let (_, r) = set(&format!("state=blocked:app=claude-code:kind=permission:msg={}", b64("approve Bash: rm x")));
    assert_eq!(r.state, ProgramState::Blocked);
    assert_eq!(r.kind, Some(BlockedKind::Permission));
    assert_eq!(r.msg.as_deref(), Some("approve Bash: rm x"));

    // Padded base64 and non-ASCII UTF-8 (`·`), exactly as captured.
    let (_, r) = set("state=blocked:app=claude-code:kind=question:msg=YW5zd2VyOiBEbyB5b3UgcHJlZmVyIHRlYSBvciBjb2ZmZWU/IChDb2ZmZWUgwrcgVGVhKQ==");
    assert_eq!(r.kind, Some(BlockedKind::Question));
    assert_eq!(r.msg.as_deref(), Some("answer: Do you prefer tea or coffee? (Coffee · Tea)"));
}

#[test]
fn padding_is_optional() {
    let (_, r) = set("state=done:msg=aGk");
    assert_eq!(r.msg.as_deref(), Some("hi"));
    let (_, r) = set("state=done:msg=aGk=");
    assert_eq!(r.msg.as_deref(), Some("hi"));
}

#[test]
fn whitespace_is_trimmed_and_the_last_repeat_wins() {
    let (_, r) = set(" state = working : app=a : app=b ");
    assert_eq!(r.state, ProgramState::Working);
    assert_eq!(r.app.as_deref(), Some("b"));
}

#[test]
fn malformed_pairs_are_skipped_and_unknown_keys_ignored() {
    // No `=`, an upper-case key, a value with a forbidden byte: each skipped.
    let (_, r) = set("garbage:State=done:app=a!b:state=idle:future=1");
    assert_eq!(r.state, ProgramState::Idle);
    assert_eq!(r.app, None);
}

#[test]
fn state_is_required_and_an_unknown_state_drops_the_report() {
    assert_eq!(parse_report(b"app=x"), None);
    assert_eq!(parse_report(b"state=sleeping"), None);
    assert_eq!(parse_report(b""), None);
}

#[test]
fn kind_only_applies_to_blocked_and_unknown_kinds_read_as_absent() {
    assert_eq!(set("state=working:kind=permission").1.kind, None);
    assert_eq!(set("state=blocked:kind=telepathy").1.kind, None);
    assert_eq!(set("state=blocked:kind=auth").1.kind, Some(BlockedKind::Auth));
}

#[test]
fn progress_is_bounded_and_only_for_working_or_blocked() {
    assert_eq!(set("state=working:progress=42").1.progress, Some(42));
    assert_eq!(set("state=working:progress=101").1.progress, None);
    assert_eq!(set("state=working:progress=x").1.progress, None);
    assert_eq!(set("state=idle:progress=42").1.progress, None);
}

#[test]
fn ids_are_slash_separated_and_a_bad_id_drops_the_report() {
    let (id, _) = set("state=working:id=build/step.1");
    assert_eq!(id, vec!["build".to_owned(), "step.1".to_owned()]);
    assert_eq!(parse_report(b"state=working:id=a//b"), None);
    assert_eq!(parse_report(b"state=working:id=a,b"), None);
    let too_long_segment = format!("state=working:id={}", "a".repeat(33));
    assert_eq!(parse_report(too_long_segment.as_bytes()), None);
    let too_deep = format!("state=working:id={}", ["a"; 9].join("/"));
    assert_eq!(parse_report(too_deep.as_bytes()), None);
    let eight_deep = format!("state=working:id={}", ["a"; 8].join("/"));
    assert!(parse_report(eight_deep.as_bytes()).is_some());
    let too_long_total = format!("state=working:id={}", vec!["a".repeat(32); 5].join("/"));
    assert_eq!(parse_report(too_long_total.as_bytes()), None);
}

#[test]
fn limit_breaches_drop_the_whole_report() {
    let key = format!("state=idle:{}=1", "k".repeat(17));
    assert_eq!(parse_report(key.as_bytes()), None);
    let app = format!("state=idle:app={}", "a".repeat(33));
    assert_eq!(parse_report(app.as_bytes()), None);
    let title = format!("state=idle:title={}", b64(&"t".repeat(193)));
    assert_eq!(parse_report(title.as_bytes()), None);
    let title_ok = format!("state=idle:title={}", b64(&"t".repeat(192)));
    assert!(parse_report(title_ok.as_bytes()).is_some());
    let msg = format!("state=idle:msg={}", b64(&"m".repeat(2049)));
    assert_eq!(parse_report(msg.as_bytes()), None);
    let msg_ok = format!("state=idle:msg={}", b64(&"m".repeat(2048)));
    assert!(parse_report(msg_ok.as_bytes()).is_some());
    let huge = format!("state=idle:pad={}", "p".repeat(4096));
    assert_eq!(parse_report(huge.as_bytes()), None);
}

#[test]
fn bad_base64_or_decoded_controls_drop_the_report() {
    // In the value charset but not base64 (`,` and `-`), or a bad length.
    assert_eq!(parse_report(b"state=idle:msg=ab,c"), None);
    assert_eq!(parse_report(b"state=idle:msg=a-b_"), None);
    assert_eq!(parse_report(b"state=idle:msg=a"), None);
    for bad in ["line\nbreak", "tab\there", "esc\u{1b}[31m", "del\u{7f}", "c1\u{9b}"] {
        let payload = format!("state=idle:msg={}", b64(bad));
        assert_eq!(parse_report(payload.as_bytes()), None, "{bad:?}");
    }
}

#[test]
fn invisible_and_bidi_characters_are_stripped_from_text() {
    let (_, r) = set(&format!("state=done:msg={}", b64("ok\u{202E}gnp.exe\u{200B}\u{2066}")));
    assert_eq!(r.msg.as_deref(), Some("okgnp.exe"));
    assert_eq!(sanitize_display_text("a\u{FEFF}b\u{061C}c"), "abc");
    assert_eq!(sanitize_display_text("plain — ünïcode ✓"), "plain — ünïcode ✓");
}

#[test]
fn a_report_replaces_its_record_whole() {
    let mut store = ProgramStatusStore::new();
    store.apply(parse_report(format!("state=blocked:kind=permission:msg={}", b64("x")).as_bytes()).unwrap());
    store.apply(parse_report(b"state=working").unwrap());
    let root = store.root().unwrap();
    assert_eq!(root.state, ProgramState::Working);
    assert_eq!(root.msg, None);
    assert_eq!(root.kind, None);
}

#[test]
fn apply_reports_only_root_changes() {
    let mut store = ProgramStatusStore::new();
    assert!(store.apply(parse_report(b"state=working").unwrap()));
    assert!(!store.apply(parse_report(b"state=working").unwrap()));
    assert!(!store.apply(parse_report(b"state=working:id=child").unwrap()));
    assert!(store.apply(parse_report(b"state=idle").unwrap()));
}

#[test]
fn clear_removes_a_record_and_its_subtree_and_bare_clear_removes_all() {
    let mut store = ProgramStatusStore::new();
    for p in ["state=idle", "state=working:id=a", "state=working:id=a/b", "state=working:id=ab"] {
        store.apply(parse_report(p.as_bytes()).unwrap());
    }
    assert!(!store.apply(parse_report(b"state=clear:id=a").unwrap()));
    assert!(store.get(&path("a")).is_none());
    assert!(store.get(&path("a/b")).is_none());
    // A sibling that merely shares a prefix string is not in the subtree.
    assert!(store.get(&path("ab")).is_some());
    assert!(store.root().is_some());
    assert!(store.apply(parse_report(b"state=clear").unwrap()));
    assert!(store.is_empty());
}

#[test]
fn app_is_inherited_from_the_nearest_ancestor() {
    let mut store = ProgramStatusStore::new();
    store.apply(parse_report(b"state=idle:app=top").unwrap());
    store.apply(parse_report(b"state=working:id=a:app=mid").unwrap());
    store.apply(parse_report(b"state=working:id=a/b/c").unwrap());
    assert_eq!(store.app_of(&path("a/b/c")), Some("mid"));
    assert_eq!(store.app_of(&path("z")), Some("top"));
    assert_eq!(store.app_of(&[]), Some("top"));
}

#[test]
fn ending_activity_keeps_done_and_error_only() {
    let mut store = ProgramStatusStore::new();
    for p in ["state=working", "state=blocked:id=b", "state=idle:id=i", "state=done:id=d", "state=error:id=e"] {
        store.apply(parse_report(p.as_bytes()).unwrap());
    }
    assert!(store.end_activity());
    assert!(store.root().is_none());
    assert!(store.get(&path("b")).is_none());
    assert!(store.get(&path("i")).is_none());
    assert!(store.get(&path("d")).is_some());
    assert!(store.get(&path("e")).is_some());

    let mut done = ProgramStatusStore::new();
    done.apply(parse_report(b"state=done").unwrap());
    assert!(!done.end_activity());
    assert_eq!(done.root().unwrap().state, ProgramState::Done);
}

#[test]
fn reset_clears_everything() {
    let mut store = ProgramStatusStore::new();
    store.apply(parse_report(b"state=done").unwrap());
    store.apply(parse_report(b"state=done:id=x").unwrap());
    assert!(store.reset());
    assert!(store.is_empty());
}

#[test]
fn the_least_recently_updated_record_is_evicted_past_the_cap() {
    let mut store = ProgramStatusStore::new();
    store.apply(parse_report(b"state=idle").unwrap());
    for i in 0..MAX_RECORDS - 1 {
        store.apply(parse_report(format!("state=working:id=r{i}").as_bytes()).unwrap());
    }
    assert_eq!(store.len(), MAX_RECORDS);
    // Touch the root so `r0` becomes the oldest, then add the 257th.
    store.apply(parse_report(b"state=working").unwrap());
    store.apply(parse_report(b"state=working:id=new").unwrap());
    assert_eq!(store.len(), MAX_RECORDS);
    assert!(store.get(&path("r0")).is_none());
    assert!(store.root().is_some());
    assert!(store.get(&path("new")).is_some());
}

#[test]
fn the_reply_gate_answers_once_and_ignores_its_own_echo() {
    let mut gate = ReplyGate::default();
    let t0 = Instant::now();
    let ticket = gate.admit(t0).expect("first query is answered");
    // Still outstanding: a second query gets nothing.
    assert!(gate.admit(t0 + Duration::from_secs(1)).is_none());
    // Written late; the echo right after is ignored, a real query later is not.
    let written = t0 + Duration::from_secs(2);
    ticket.mark_written(written);
    assert!(gate.admit(written + Duration::from_millis(30)).is_none());
    assert!(gate.admit(written + Duration::from_millis(600)).is_some());
}

#[test]
fn the_reply_gate_caps_a_flood() {
    let mut gate = ReplyGate::default();
    let t0 = Instant::now();
    let mut answered = 0;
    for i in 0..200u64 {
        let now = t0 + Duration::from_millis(i * 700);
        if let Some(t) = gate.admit(now) {
            t.mark_written(now);
            answered += 1;
        }
    }
    // 140 s of queries: at most 8 per started minute.
    assert!((8..=24).contains(&answered), "{answered}");
}

// --- Through `TerminalState`: the path every backend uses. ---

mod terminal_state {
    use crate::backend::TerminalSessionId;
    use crate::events::TerminalEvent;
    use crate::program_status::{ProgramRecord, ProgramState};
    use crate::state::TerminalState;

    const ID: TerminalSessionId = TerminalSessionId(3);
    const QUERY: &[u8] = b"\x1b]7501;?\x07";

    fn roots(events: &[TerminalEvent]) -> Vec<Option<ProgramRecord>> {
        events
            .iter()
            .filter_map(|e| match e {
                TerminalEvent::ProgramStatus { root, .. } => Some(root.clone()),
                _ => None,
            })
            .collect()
    }

    fn state_of(root: &Option<ProgramRecord>) -> Option<ProgramState> {
        root.as_ref().map(|r| r.state)
    }

    #[test]
    fn a_mirror_never_answers_and_never_emits_a_pty_reply_for_the_query() {
        let mut t = TerminalState::new(80, 24, 100);
        let events = t.advance_collecting(ID, QUERY);
        assert!(t.take_program_status_reply().is_none());
        assert!(!events.iter().any(|e| matches!(e, TerminalEvent::PtyReply { .. })));
    }

    #[test]
    fn an_owner_answers_a_live_query_once() {
        let mut t = TerminalState::new(80, 24, 100);
        t.set_answers_program_status(true);
        let events = t.advance_collecting(ID, QUERY);
        assert!(!events.iter().any(|e| matches!(e, TerminalEvent::PtyReply { .. })));
        assert!(t.take_program_status_reply().is_some());
        assert!(t.take_program_status_reply().is_none());
    }

    #[test]
    fn reports_split_across_reads_emit_with_the_handshake() {
        let mut t = TerminalState::new(80, 24, 100);
        t.advance_collecting(ID, QUERY);
        let a = t.advance_collecting(ID, b"\x1b]7501;state=wor");
        assert!(roots(&a).is_empty());
        let b = t.advance_collecting(ID, b"king:app=x\x07");
        assert_eq!(roots(&b).iter().map(state_of).collect::<Vec<_>>(), vec![Some(ProgramState::Working)]);
        assert!(b.iter().any(|e| matches!(e, TerminalEvent::ProgramStatus { handshake: true, .. })));
    }

    #[test]
    fn a_report_without_a_query_carries_no_handshake() {
        let mut t = TerminalState::new(80, 24, 100);
        let events = t.advance_collecting(ID, b"\x1b]7501;state=blocked:kind=permission\x07");
        assert!(events.iter().any(|e| matches!(e, TerminalEvent::ProgramStatus { handshake: false, .. })));
    }

    #[test]
    fn clear_keeps_records_but_reset_and_a_prompt_release_them() {
        let mut t = TerminalState::new(80, 24, 100);
        t.advance_collecting(ID, b"\x1b]7501;state=working\x07");
        // `clear(1)`: erase saved lines, home, erase screen.
        assert!(roots(&t.advance_collecting(ID, b"\x1b[3J\x1b[H\x1b[2J")).is_empty());
        let reset = t.advance_collecting(ID, b"\x1bc");
        assert_eq!(roots(&reset), vec![None]);

        t.advance_collecting(ID, b"\x1b]7501;state=done\x07");
        let prompt = t.advance_collecting(ID, b"\x1b]133;A\x07$ ");
        assert_eq!(roots(&prompt), vec![None]);
    }

    #[test]
    fn a_replay_restores_the_root_without_answering_or_emitting() {
        let mut live = TerminalState::new(80, 24, 100);
        live.set_answers_program_status(true);
        let mut ring = QUERY.to_vec();
        ring.extend_from_slice(b"hello\r\n\x1b]7501;state=blocked:kind=question\x07");
        live.replay(&ring);
        assert!(live.take_program_status_reply().is_none());
        match live.program_status_event(ID) {
            Some(TerminalEvent::ProgramStatus { root: Some(r), handshake: true, .. }) => {
                assert_eq!(r.state, ProgramState::Blocked)
            }
            other => panic!("expected the replayed root, got {other:?}"),
        }
        // A replay whose ring lost the query still restores the root.
        let mut cut = TerminalState::new(80, 24, 100);
        cut.replay(b"\x1b]7501;state=working\x07");
        assert!(matches!(
            cut.program_status_event(ID),
            Some(TerminalEvent::ProgramStatus { root: Some(_), handshake: false, .. })
        ));
        // A live query after the replay is still answered.
        live.advance_collecting(ID, QUERY);
        assert!(live.take_program_status_reply().is_some());
    }

    #[test]
    fn a_rebuilt_state_inherits_owner_and_handshake() {
        let mut old = TerminalState::new(80, 24, 100);
        old.set_answers_program_status(true);
        old.advance_collecting(ID, QUERY);
        let mut fresh = TerminalState::new(80, 24, 100);
        fresh.inherit_program_status(&old);
        fresh.replay(b"\x1b]7501;state=idle\x07");
        assert!(matches!(
            fresh.program_status_event(ID),
            Some(TerminalEvent::ProgramStatus { handshake: true, .. })
        ));
        fresh.advance_collecting(ID, QUERY);
        assert!(fresh.take_program_status_reply().is_some());
    }

    #[test]
    fn a_rebuild_announces_only_a_change_that_fell_in_the_gap() {
        let mut old = TerminalState::new(80, 24, 100);
        old.advance_collecting(ID, b"\x1b]7501;state=working\x07");
        let before = old.program_status_root();

        let mut same = TerminalState::new(80, 24, 100);
        same.replay(b"\x1b]7501;state=working\x07");
        assert!(same.program_status_change_since(ID, before.as_ref()).is_none());

        let mut moved = TerminalState::new(80, 24, 100);
        moved.replay(b"\x1b]7501;state=working\x07\x1b]7501;state=blocked:kind=question\x07");
        assert!(matches!(
            moved.program_status_change_since(ID, before.as_ref()),
            Some(TerminalEvent::ProgramStatus { root: Some(r), .. }) if r.state == ProgramState::Blocked
        ));

        // A prompt in the gap: the program finished, so consumers are released.
        let mut finished = TerminalState::new(80, 24, 100);
        finished.replay(b"\x1b]7501;state=done\x07\x1b]133;A\x07");
        assert!(matches!(
            finished.program_status_change_since(ID, before.as_ref()),
            Some(TerminalEvent::ProgramStatus { root: None, .. })
        ));
        assert!(finished.program_status_event(ID).is_none(), "nothing to seed after a release");
    }
}
