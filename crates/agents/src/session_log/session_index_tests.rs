//! Unit tests for [`super`] (the session index). Split out via `#[path]`
//! to keep `session_index.rs` under the file-size cap.

    use super::*;

    fn claude_user_line(ts: &str, cwd: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","cwd":"{cwd}","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn claude_assistant_line(ts: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"text","text":"ok"}}]}}}}"#
        )
    }

    #[test]
    fn session_index_parses_jsonl_fixture() {
        let content = format!(
            "{}\n{}\n",
            claude_user_line("2026-06-20T10:00:00Z", "/Users/x/proj", "refactor the parser"),
            claude_assistant_line("2026-06-20T10:05:00Z"),
        );
        let e = parse_claude_jsonl("sess-uuid", &content).unwrap();
        assert_eq!(e.session_id, "sess-uuid");
        assert_eq!(e.adapter, AgentAdapter::ClaudeCode);
        assert_eq!(e.cwd.as_deref(), Some("/Users/x/proj"));
        assert_eq!(e.title.as_deref(), Some("refactor the parser"));
        assert_eq!(e.last_message_ts_ms, parse_timestamp_ms("2026-06-20T10:05:00Z"));
        assert_eq!(e.entry_count, Some(2));
    }

    #[test]
    fn claude_first_prompt_reads_array_content_block() {
        let line = r#"{"type":"user","timestamp":"2026-06-20T10:00:00Z","message":{"content":[{"type":"text","text":"hello there"}]}}"#;
        let e = parse_claude_jsonl("s", line).unwrap();
        assert_eq!(e.title.as_deref(), Some("hello there"));
    }

    #[test]
    fn claude_first_prompt_collapses_whitespace() {
        let line = claude_user_line("2026-06-20T10:00:00Z", "/p", "line one\\nline   two");
        let e = parse_claude_jsonl("s", &line).unwrap();
        // The literal "\n" in JSON decodes to a newline; preview flattens it.
        assert_eq!(e.title.as_deref(), Some("line one line two"));
    }

    #[test]
    fn title_prefers_last_prompt_over_first_user_message() {
        let last_prompt = r#"{"type":"last-prompt","lastPrompt":"/cook plan.md","sessionId":"s"}"#;
        let content = format!(
            "{last_prompt}\n{}\n",
            claude_user_line("2026-06-20T10:00:00Z", "/p", "the first message"),
        );
        let e = parse_claude_jsonl("s", &content).unwrap();
        assert_eq!(e.title.as_deref(), Some("/cook plan.md"));
    }

    #[test]
    fn title_prefers_custom_then_ai_title() {
        // customTitle (a user rename) wins over aiTitle and the first prompt.
        let content = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"aiTitle","aiTitle":"Auto summary"}"#,
            claude_user_line("2026-06-20T10:00:00Z", "/p", "hi"),
            r#"{"type":"customTitle","customTitle":"My renamed session"}"#,
        );
        let e = parse_claude_jsonl("s", &content).unwrap();
        assert_eq!(e.title.as_deref(), Some("My renamed session"));
        assert_eq!(e.custom_title.as_deref(), Some("My renamed session"));

        // Without a customTitle, aiTitle wins over the "hi" first prompt.
        let content2 = format!(
            "{}\n{}\n",
            r#"{"type":"aiTitle","aiTitle":"Refactor the parser"}"#,
            claude_user_line("2026-06-20T10:00:00Z", "/p", "hi"),
        );
        let e2 = parse_claude_jsonl("s", &content2).unwrap();
        assert_eq!(e2.title.as_deref(), Some("Refactor the parser"));
    }

    #[test]
    fn enriches_tag_created_at_and_message_count() {
        let content = format!(
            "{}\n{}\n{}\n",
            claude_user_line("2026-06-20T10:00:00Z", "/p", "first"),
            claude_assistant_line("2026-06-20T10:01:00Z"),
            r#"{"type":"tag","tag":"experiment"}"#,
        );
        let e = parse_claude_jsonl("s", &content).unwrap();
        assert_eq!(e.tag.as_deref(), Some("experiment"));
        assert_eq!(e.created_at_ms, parse_timestamp_ms("2026-06-20T10:00:00Z"));
        // 1 user + 1 assistant (the tag line is neither).
        assert_eq!(e.message_count, Some(2));
    }

    #[test]
    fn title_unwraps_slash_command_xml() {
        // A slash-command user message with no last-prompt line falls back to
        // the unwrapped first message: "/cook <args>", not the raw tags.
        let xml = "<command-message>cook</command-message><command-name>/cook</command-name><command-args>plan.md here</command-args>";
        let line = claude_user_line("2026-06-20T10:00:00Z", "/p", xml);
        let e = parse_claude_jsonl("s", &line).unwrap();
        assert_eq!(e.title.as_deref(), Some("/cook plan.md here"));
    }

    #[test]
    fn parses_git_branch_and_size() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude").join("projects").join("-p");
        std::fs::create_dir_all(&proj).unwrap();
        let line = format!(
            r#"{{"type":"user","timestamp":"2026-06-20T10:00:00Z","cwd":"/p","gitBranch":"feat/x","message":{{"content":"hi"}}}}"#
        );
        let path = proj.join("s.jsonl");
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let entries = SessionIndex::build(
            &tmp.path().join(".claude"),
            Path::new("/nonexistent"),
            &SessionScope::AllProjects,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].git_branch.as_deref(), Some("feat/x"));
        assert_eq!(entries[0].size_bytes, Some(std::fs::metadata(&path).unwrap().len()));
    }

    #[test]
    fn parse_codex_index_line_reads_id_thread_updated() {
        let line = r#"{"id":"abc-123","thread_name":"fix flaky test","updated_at":"2026-06-19T09:00:00Z"}"#;
        let e = parse_codex_index_line(line).unwrap();
        assert_eq!(e.session_id, "abc-123");
        assert_eq!(e.adapter, AgentAdapter::Codex);
        assert_eq!(e.cwd, None);
        assert_eq!(e.title.as_deref(), Some("fix flaky test"));
        assert_eq!(e.last_message_ts_ms, parse_timestamp_ms("2026-06-19T09:00:00Z"));
    }

    #[test]
    fn parse_codex_index_line_accepts_epoch_seconds() {
        let line = r#"{"id":"x","thread_name":"t","updated_at":1718784000}"#;
        let e = parse_codex_index_line(line).unwrap();
        assert_eq!(e.last_message_ts_ms, Some(1_718_784_000_000));
    }

    #[test]
    fn malformed_lines_are_skipped_without_panic() {
        assert!(parse_claude_jsonl("s", "not json\n{bad").is_none());
        assert!(parse_codex_index_line("garbage").is_none());
        assert!(parse_codex_index_line(r#"{"thread_name":"no id"}"#).is_none());
        assert!(parse_codex_index_line(r#"{"id":""}"#).is_none());
    }

    #[test]
    fn build_scans_claude_and_codex_sorted_desc() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        let codex = tmp.path().join(".codex");
        let proj = claude.join("projects").join("-Users-x-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&codex).unwrap();

        std::fs::write(
            proj.join("old.jsonl"),
            format!("{}\n", claude_user_line("2026-06-18T08:00:00Z", "/Users/x/proj", "old task")),
        )
        .unwrap();
        std::fs::write(
            proj.join("new.jsonl"),
            format!("{}\n", claude_user_line("2026-06-20T08:00:00Z", "/Users/x/proj", "new task")),
        )
        .unwrap();
        std::fs::write(
            codex.join("session_index.jsonl"),
            "{\"id\":\"cx\",\"thread_name\":\"codex task\",\"updated_at\":\"2026-06-19T08:00:00Z\"}\n",
        )
        .unwrap();

        let entries = SessionIndex::build(&claude, &codex, &SessionScope::AllProjects);
        assert_eq!(entries.len(), 3);
        // Newest first: new (06-20) > codex (06-19) > old (06-18).
        assert_eq!(entries[0].session_id, "new");
        assert_eq!(entries[0].title.as_deref(), Some("new task"));
        assert_eq!(entries[1].session_id, "cx");
        assert_eq!(entries[1].adapter, AgentAdapter::Codex);
        assert_eq!(entries[2].session_id, "old");
    }

    #[test]
    fn build_missing_dirs_is_empty() {
        let entries = SessionIndex::build(
            Path::new("/nonexistent/claude"),
            Path::new("/nonexistent/codex"),
            &SessionScope::AllProjects,
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn sanitize_project_path_matches_claude_slug() {
        assert_eq!(
            sanitize_project_path("/Users/x/Code/projects/OxiMux"),
            "-Users-x-Code-projects-OxiMux"
        );
        // Every non-alphanumeric byte → '-' (spaces, dots, colons included).
        assert_eq!(sanitize_project_path("/a/My App.v2"), "-a-My-App-v2");
    }

    #[test]
    fn scoped_build_includes_only_the_matching_project() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        let proj_a = claude
            .join("projects")
            .join(sanitize_project_path("/Users/x/proj-a"));
        let proj_b = claude
            .join("projects")
            .join(sanitize_project_path("/Users/x/proj-b"));
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();
        std::fs::write(
            proj_a.join("a.jsonl"),
            format!("{}\n", claude_user_line("2026-06-20T08:00:00Z", "/Users/x/proj-a", "in a")),
        )
        .unwrap();
        std::fs::write(
            proj_b.join("b.jsonl"),
            format!("{}\n", claude_user_line("2026-06-20T08:00:00Z", "/Users/x/proj-b", "in b")),
        )
        .unwrap();

        // Scoped to proj-a → only proj-a's session.
        let scoped = SessionIndex::build(
            &claude,
            Path::new("/none"),
            &SessionScope::Projects(vec!["/Users/x/proj-a".to_string()]),
        );
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].session_id, "a");

        // All projects → both.
        let all = SessionIndex::build(&claude, Path::new("/none"), &SessionScope::AllProjects);
        assert_eq!(all.len(), 2);

        // Empty scope matches nothing (callers pass AllProjects instead).
        let none =
            SessionIndex::build(&claude, Path::new("/none"), &SessionScope::Projects(vec![]));
        assert!(none.is_empty());
    }

    #[test]
    fn scoped_build_excludes_codex_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        let codex = tmp.path().join(".codex");
        let proj = claude
            .join("projects")
            .join(sanitize_project_path("/Users/x/proj"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            proj.join("a.jsonl"),
            format!("{}\n", claude_user_line("2026-06-20T08:00:00Z", "/Users/x/proj", "hi")),
        )
        .unwrap();
        std::fs::write(
            codex.join("session_index.jsonl"),
            "{\"id\":\"cx\",\"thread_name\":\"t\",\"updated_at\":\"2026-06-19T08:00:00Z\"}\n",
        )
        .unwrap();

        // Codex has no cwd → only the all view shows it.
        let scoped = SessionIndex::build(
            &claude,
            &codex,
            &SessionScope::Projects(vec!["/Users/x/proj".to_string()]),
        );
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].adapter, AgentAdapter::ClaudeCode);

        let all = SessionIndex::build(&claude, &codex, &SessionScope::AllProjects);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn large_claude_log_uses_bounded_path_with_no_entry_count() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude").join("projects").join("-p");
        std::fs::create_dir_all(&proj).unwrap();

        let mut content =
            format!("{}\n", claude_user_line("2026-06-20T08:00:00Z", "/p", "kick off"));
        let filler = claude_assistant_line("2026-06-20T08:01:00Z");
        while (content.len() as u64) <= FULL_PARSE_LIMIT + 4096 {
            content.push_str(&filler);
            content.push('\n');
        }
        // A recognizable newest timestamp at the very end (within the tail).
        content.push_str(&format!("{}\n", claude_assistant_line("2026-06-20T09:00:00Z")));
        std::fs::write(proj.join("big.jsonl"), &content).unwrap();

        let entries = SessionIndex::build(
            &tmp.path().join(".claude"),
            Path::new("/nonexistent"),
            &SessionScope::AllProjects,
        );
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.session_id, "big");
        assert_eq!(e.title.as_deref(), Some("kick off"));
        assert_eq!(e.last_message_ts_ms, parse_timestamp_ms("2026-06-20T09:00:00Z"));
        assert_eq!(e.entry_count, None);
    }
