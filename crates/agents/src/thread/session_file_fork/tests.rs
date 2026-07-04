use super::*;

/// Build a realistic session file: meta lines, real user turns, assistant
/// turns (thinking + text as separate lines sharing message.id), tool_use /
/// tool_result pairs, a compaction summary, and sessionId on every line that
/// carries one in the wild.
fn fixture_lines(sid: &str) -> Vec<String> {
    let l = |json: serde_json::Value| json.to_string();
    vec![
        l(serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":sid})),
        l(serde_json::json!({"type":"mode","mode":"normal","sessionId":sid})),
        // user #0
        l(serde_json::json!({"type":"user","uuid":"u0","sessionId":sid,
            "message":{"role":"user","content":"first question"}})),
        l(serde_json::json!({"type":"assistant","uuid":"a0","sessionId":sid,
            "message":{"id":"msg_0","role":"assistant","content":[{"type":"thinking","thinking":"hm","signature":"sig"}]}})),
        l(serde_json::json!({"type":"assistant","uuid":"a1","sessionId":sid,
            "message":{"id":"msg_0","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}})),
        // tool_result carrier — type user but NOT a real user line
        l(serde_json::json!({"type":"user","uuid":"u-tr","sessionId":sid,
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"body"}]}})),
        l(serde_json::json!({"type":"assistant","uuid":"a2","sessionId":sid,
            "message":{"id":"msg_0","role":"assistant","content":[{"type":"text","text":"answer one"}]}})),
        // compaction summary — type user but NOT a real user line
        l(serde_json::json!({"type":"user","uuid":"u-cs","sessionId":sid,"isCompactSummary":true,
            "message":{"role":"user","content":"summary of earlier context"}})),
        // sidechain line — NOT a real user line
        l(serde_json::json!({"type":"user","uuid":"u-sc","sessionId":sid,"isSidechain":true,
            "message":{"role":"user","content":"subagent prompt"}})),
        // user #1 (content-array form with text + image)
        l(serde_json::json!({"type":"user","uuid":"u1","sessionId":sid,
            "message":{"role":"user","content":[{"type":"text","text":"second question"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}]}})),
        l(serde_json::json!({"type":"assistant","uuid":"a3","sessionId":sid,
            "message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"answer two"}]}})),
        l(serde_json::json!({"type":"last-prompt","lastPrompt":"second question","sessionId":sid})),
        // user #2
        l(serde_json::json!({"type":"user","uuid":"u2","sessionId":sid,
            "message":{"role":"user","content":"third question"}})),
        l(serde_json::json!({"type":"assistant","uuid":"a4","sessionId":sid,
            "message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"answer three"}]}})),
    ]
}

fn write_session(root: &Path, sid: &str, lines: &[String]) -> PathBuf {
    let dir = root.join("-Users-test-project");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn parsed_lines(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn forks_at_second_user_message() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_session(tmp.path(), "sid-a", &fixture_lines("sid-a"));
    let before = std::fs::read(&src).unwrap();

    let new_sid = fork_truncated(tmp.path(), "sid-a", 1, "second question").unwrap();

    // Original byte-identical.
    assert_eq!(std::fs::read(&src).unwrap(), before);

    let dst = src.with_file_name(format!("{new_sid}.jsonl"));
    let lines = parsed_lines(&dst);
    // Kept: everything before user #1 — meta(2) + u0 + a0 + a1 + u-tr + a2 + u-cs + u-sc = 9
    assert_eq!(lines.len(), 9);
    // Every kept line that carries a sessionId now carries the NEW one.
    for v in &lines {
        if let Some(s) = v.get("sessionId") {
            assert_eq!(s.as_str().unwrap(), new_sid);
        }
    }
    // The dropped content is gone.
    let text = std::fs::read_to_string(&dst).unwrap();
    assert!(!text.contains("second question"));
    assert!(!text.contains("answer two"));
    // Non-real user lines were NOT miscounted as ordinals.
    assert!(text.contains("summary of earlier context"), "compact summary kept");
    assert!(text.contains("tool_use_id"), "tool_result carrier kept");
}

#[test]
fn ordinal_counts_skip_tool_results_compaction_sidechains() {
    let tmp = tempfile::tempdir().unwrap();
    write_session(tmp.path(), "sid-b", &fixture_lines("sid-b"));
    // Ordinal 2 = "third question" despite 3 intervening user-typed lines.
    let new_sid = fork_truncated(tmp.path(), "sid-b", 2, "third question").unwrap();
    let dst = locate_session_file(tmp.path(), &new_sid).unwrap();
    let text = std::fs::read_to_string(&dst).unwrap();
    assert!(text.contains("second question"), "user #1 kept");
    assert!(!text.contains("third question"), "user #2 removed");
}

#[test]
fn text_mismatch_aborts_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_session(tmp.path(), "sid-c", &fixture_lines("sid-c"));
    let siblings_before = src.parent().unwrap().read_dir().unwrap().count();

    let err = fork_truncated(tmp.path(), "sid-c", 1, "totally different text").unwrap_err();
    assert!(matches!(err, ForkError::OrdinalMismatch { ordinal: 1 }));
    assert_eq!(
        src.parent().unwrap().read_dir().unwrap().count(),
        siblings_before,
        "no file written on mismatch"
    );
}

#[test]
fn ordinal_out_of_range_reports_count() {
    let tmp = tempfile::tempdir().unwrap();
    write_session(tmp.path(), "sid-d", &fixture_lines("sid-d"));
    let err = fork_truncated(tmp.path(), "sid-d", 9, "whatever").unwrap_err();
    match err {
        ForkError::OrdinalOutOfRange { ordinal: 9, found } => assert_eq!(found, 3),
        other => panic!("expected OrdinalOutOfRange, got {other:?}"),
    }
}

#[test]
fn torn_tail_beyond_cut_is_tolerated() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = fixture_lines("sid-e");
    lines.push(r#"{"type":"assistant","uuid":"a-torn","sessionId":"sid-e","mess"#.to_string());
    write_session(tmp.path(), "sid-e", &lines);
    // Cutting at user #1: the torn final line is beyond the cut → fine.
    fork_truncated(tmp.path(), "sid-e", 1, "second question").expect("torn tail tolerated");
}

#[test]
fn torn_line_before_cut_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = fixture_lines("sid-f");
    // Corrupt an EARLY line (inside the kept region).
    lines[3] = lines[3][..lines[3].len() - 10].to_string();
    write_session(tmp.path(), "sid-f", &lines);
    let err = fork_truncated(tmp.path(), "sid-f", 1, "second question").unwrap_err();
    assert!(matches!(err, ForkError::TornFile));
}

#[test]
fn missing_session_reports_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let err = fork_truncated(tmp.path(), "sid-none", 0, "x").unwrap_err();
    assert!(matches!(err, ForkError::SessionFileNotFound { .. }));
}

#[test]
fn whitespace_normalized_prefix_match() {
    assert!(text_matches("hello   world\nand more", "hello world"));
    assert!(text_matches("exact", "exact"));
    assert!(!text_matches("different", "hello"));
    assert!(!text_matches("", "expected"));
    assert!(text_matches("", ""));
}

#[test]
fn writes_a_distinct_new_file_each_fork() {
    // Two forks of the same source produce two different session files, and
    // neither overwrites the other (create_new). We can't force a uuid
    // collision, so we assert distinctness + that both files exist.
    let tmp = tempfile::tempdir().unwrap();
    let src = write_session(tmp.path(), "sid-g", &fixture_lines("sid-g"));
    let a = fork_truncated(tmp.path(), "sid-g", 1, "second question").unwrap();
    let b = fork_truncated(tmp.path(), "sid-g", 1, "second question").unwrap();
    assert_ne!(a, b, "each fork mints a fresh session id");
    assert!(src.with_file_name(format!("{a}.jsonl")).exists());
    assert!(src.with_file_name(format!("{b}.jsonl")).exists());
}

#[test]
fn locate_finds_across_project_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("-Users-other-proj");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("sid-z.jsonl"), "{}\n").unwrap();
    assert!(locate_session_file(tmp.path(), "sid-z").is_some());
    assert!(locate_session_file(tmp.path(), "sid-missing").is_none());
}
