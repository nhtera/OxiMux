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
        let line =
            r#"{"type":"user","timestamp":"2026-06-20T10:00:00Z","cwd":"/p","gitBranch":"feat/x","message":{"content":"hi"}}"#
                .to_string();
        let path = proj.join("s.jsonl");
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let entries = SessionIndex::build(
            &tmp.path().join(".claude"),
            Path::new("/nonexistent"),
            Path::new("/nonexistent-home"),
            &SessionScope::AllProjects,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].git_branch.as_deref(), Some("feat/x"));
        assert_eq!(entries[0].size_bytes, Some(std::fs::metadata(&path).unwrap().len()));
    }

    /// A minimal Codex rollout: `session_meta` head + optional injected preamble
    /// turns + the genuine first user prompt. Mirrors the real on-disk shape.
    fn codex_rollout(ts: &str, cwd: &str, branch: &str, id: &str, prompt: &str) -> String {
        let meta = format!(
            r#"{{"timestamp":"{ts}","type":"session_meta","payload":{{"session_id":"{id}","cwd":"{cwd}","timestamp":"{ts}","git":{{"branch":"{branch}"}}}}}}"#
        );
        // Injected synthetic-user turns Codex prepends, which must be skipped.
        // `r##` delimiter: the `"#` in `:"# AGENTS` would close a plain `r#`.
        let agents = format!(
            r##"{{"timestamp":"{ts}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"# AGENTS.md instructions for /x — lots of text"}}]}}}}"##
        );
        let env = format!(
            r#"{{"timestamp":"{ts}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"<environment_context><cwd>{cwd}</cwd></environment_context>"}}]}}}}"#
        );
        let user = format!(
            r#"{{"timestamp":"{ts}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{prompt}"}}]}}}}"#
        );
        format!("{meta}\n{agents}\n{env}\n{user}\n")
    }

    fn write_codex_rollout(codex: &Path, day: &str, file: &str, body: &str) {
        let dir = codex.join("sessions").join(day);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), body).unwrap();
    }

    #[test]
    fn codex_rollout_reads_meta_and_first_genuine_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let codex = tmp.path().join(".codex");
        write_codex_rollout(
            &codex,
            "2026/06/19",
            "rollout-2026-06-19-abc-123.jsonl",
            &codex_rollout(
                "2026-06-19T09:00:00Z",
                "/Users/x/proj",
                "main",
                "abc-123",
                "fix flaky test",
            ),
        );
        let entries = SessionIndex::build(
            Path::new("/none"),
            &codex,
            Path::new("/nonexistent-home"),
            &SessionScope::AllProjects,
        );
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.session_id, "abc-123");
        assert_eq!(e.adapter, AgentAdapter::Codex);
        assert_eq!(e.cwd.as_deref(), Some("/Users/x/proj"));
        assert_eq!(e.git_branch.as_deref(), Some("main"));
        // The injected AGENTS.md + environment_context turns are skipped.
        assert_eq!(e.title.as_deref(), Some("fix flaky test"));
        assert_eq!(e.last_message_ts_ms, parse_timestamp_ms("2026-06-19T09:00:00Z"));
    }

    #[test]
    fn codex_rollout_reads_legacy_id_and_top_level_message() {
        // Pre-0.14 CLI: meta keys the id as `id`, and turns are top-level
        // `type: "message"` lines (no `response_item` wrapper).
        let tmp = tempfile::tempdir().unwrap();
        let codex = tmp.path().join(".codex");
        let body = concat!(
            r#"{"type":"session_meta","payload":{"id":"legacy-1","cwd":"/Users/x/proj","timestamp":"2025-08-29T02:29:19Z","git":{"branch":"dev"}}}"#,
            "\n",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context><cwd>/Users/x/proj</cwd></environment_context>"}]}"#,
            "\n",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"port the module"}]}"#,
            "\n",
        );
        write_codex_rollout(&codex, "2025/08/29", "rollout-2025-08-29-legacy-1.jsonl", body);
        let entries = SessionIndex::build(
            Path::new("/none"),
            &codex,
            Path::new("/nonexistent-home"),
            &SessionScope::AllProjects,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "legacy-1");
        assert_eq!(entries[0].git_branch.as_deref(), Some("dev"));
        assert_eq!(entries[0].title.as_deref(), Some("port the module"));
    }

    #[test]
    fn codex_rollout_without_session_meta_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let codex = tmp.path().join(".codex");
        write_codex_rollout(
            &codex,
            "2026/06/19",
            "rollout-2026-06-19-noid.jsonl",
            "{\"type\":\"event_msg\",\"payload\":{}}\n",
        );
        let entries = SessionIndex::build(
            Path::new("/none"),
            &codex,
            Path::new("/nonexistent-home"),
            &SessionScope::AllProjects,
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped_without_panic() {
        assert!(parse_claude_jsonl("s", "not json\n{bad").is_none());
        let tmp = tempfile::tempdir().unwrap();
        let codex = tmp.path().join(".codex");
        // A rollout of pure noise yields no entry, never a panic.
        write_codex_rollout(&codex, "2026/06/19", "rollout-garbage.jsonl", "garbage\n{bad\n");
        let entries = SessionIndex::build(
            Path::new("/none"),
            &codex,
            Path::new("/nonexistent-home"),
            &SessionScope::AllProjects,
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn build_scans_claude_and_codex_sorted_desc() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        let codex = tmp.path().join(".codex");
        let proj = claude.join("projects").join("-Users-x-proj");
        std::fs::create_dir_all(&proj).unwrap();

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
        write_codex_rollout(
            &codex,
            "2026/06/19",
            "rollout-2026-06-19-cx.jsonl",
            &codex_rollout(
                "2026-06-19T08:00:00Z",
                "/Users/x/proj",
                "main",
                "cx",
                "codex task",
            ),
        );

        let entries = SessionIndex::build(
            &claude,
            &codex,
            Path::new("/nonexistent-home"),
            &SessionScope::AllProjects,
        );
        assert_eq!(entries.len(), 3);
        // Newest first: new (06-20) > codex (06-19) > old (06-18).
        assert_eq!(entries[0].session_id, "new");
        assert_eq!(entries[0].title.as_deref(), Some("new task"));
        assert_eq!(entries[1].session_id, "cx");
        assert_eq!(entries[1].adapter, AgentAdapter::Codex);
        assert_eq!(entries[1].title.as_deref(), Some("codex task"));
        assert_eq!(entries[2].session_id, "old");
    }

    #[test]
    fn build_missing_dirs_is_empty() {
        let entries = SessionIndex::build(
            Path::new("/nonexistent/claude"),
            Path::new("/nonexistent/codex"),
            Path::new("/nonexistent/home"),
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
            Path::new("/nonexistent-home"),
            &SessionScope::Projects(vec!["/Users/x/proj-a".to_string()]),
        );
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].session_id, "a");

        // All projects → both.
        let all = SessionIndex::build(
            &claude,
            Path::new("/none"),
            Path::new("/nonexistent-home"),
            &SessionScope::AllProjects,
        );
        assert_eq!(all.len(), 2);

        // Empty scope matches nothing (callers pass AllProjects instead).
        let none = SessionIndex::build(
            &claude,
            Path::new("/none"),
            Path::new("/nonexistent-home"),
            &SessionScope::Projects(vec![]),
        );
        assert!(none.is_empty());
    }

    #[test]
    fn scoped_build_filters_codex_by_recorded_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        let codex = tmp.path().join(".codex");
        let proj = claude
            .join("projects")
            .join(sanitize_project_path("/Users/x/proj"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("a.jsonl"),
            format!("{}\n", claude_user_line("2026-06-20T08:00:00Z", "/Users/x/proj", "hi")),
        )
        .unwrap();
        // One Codex session in the active project, one elsewhere.
        write_codex_rollout(
            &codex,
            "2026/06/19",
            "rollout-2026-06-19-here.jsonl",
            &codex_rollout("2026-06-19T08:00:00Z", "/Users/x/proj", "main", "here", "in proj"),
        );
        write_codex_rollout(
            &codex,
            "2026/06/19",
            "rollout-2026-06-19-there.jsonl",
            &codex_rollout("2026-06-19T08:00:00Z", "/Users/x/other", "main", "there", "elsewhere"),
        );

        // Scoped to /Users/x/proj → the Claude session + the matching-cwd Codex
        // one; the other-cwd Codex session is excluded.
        let scoped = SessionIndex::build(
            &claude,
            &codex,
            Path::new("/nonexistent-home"),
            &SessionScope::Projects(vec!["/Users/x/proj".to_string()]),
        );
        assert_eq!(scoped.len(), 2);
        assert!(scoped.iter().any(|e| e.session_id == "here"));
        assert!(scoped.iter().all(|e| e.session_id != "there"));

        // All projects → every session regardless of cwd.
        let all = SessionIndex::build(
            &claude,
            &codex,
            Path::new("/nonexistent-home"),
            &SessionScope::AllProjects,
        );
        assert_eq!(all.len(), 3);
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
            Path::new("/nonexistent-home"),
            &SessionScope::AllProjects,
        );
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.session_id, "big");
        assert_eq!(e.title.as_deref(), Some("kick off"));
        assert_eq!(e.last_message_ts_ms, parse_timestamp_ms("2026-06-20T09:00:00Z"));
        assert_eq!(e.entry_count, None);
    }
