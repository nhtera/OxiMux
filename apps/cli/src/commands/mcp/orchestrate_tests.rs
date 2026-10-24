//! Delegation tool tests: what is offered to whom, the wait's reading of a
//! child's events, and the wording.

use oximux_remote_proto::lineage::AgentRosterWire;
use oximux_remote_proto::messages::SessionStatusWire;

use super::*;

fn me(spawn_allowed: bool) -> WhoamiWire {
    WhoamiWire {
        scope: "session".into(),
        session: Some("claude-parent".into()),
        depth: 0,
        root_id: Some("claude-parent".into()),
        spawn_allowed,
        reason: (!spawn_allowed).then(|| "depth 1 of 1".into()),
        agents: vec![
            AgentRosterWire {
                agent_id: "claude-code".into(),
                display: "Claude Code".into(),
                default_model: None,
                models: vec![],
                postures: vec![],
                installed: true,
            },
            AgentRosterWire {
                agent_id: "gemini".into(),
                display: "Gemini".into(),
                default_model: None,
                models: vec![],
                postures: vec![],
                installed: false,
            },
        ],
    }
}

fn names(tools: &[Value]) -> Vec<&str> {
    tools.iter().map(|t| t["name"].as_str().unwrap()).collect()
}

#[test]
fn the_spawn_tool_is_offered_only_when_the_host_allows_it() {
    let allowed = tools_for(Some(&me(true)), Some("claude-code"));
    assert_eq!(names(&allowed)[0], "oximux_spawn_agent");
    for refused in [tools_for(Some(&me(false)), Some("claude-code")), tools_for(None, None)] {
        let names = names(&refused);
        assert!(!names.contains(&"oximux_spawn_agent"), "{names:?}");
        // Everything else stays: a child at the depth cap still waits on and
        // reads what it has.
        assert!(names.contains(&"oximux_wait_agents"));
        assert!(names.contains(&"oximux_list_children"));
    }
    assert!(names(&allowed).iter().all(|n| n.starts_with(PREFIX)));
}

#[test]
fn the_spawn_tool_names_installed_agents_and_the_default() {
    let tools = tools_for(Some(&me(true)), Some("claude-code"));
    let spawn = &tools[0];
    let description = spawn["description"].as_str().unwrap();
    assert!(description.contains("Agents: claude-code."), "{description}");
    assert!(!description.contains("gemini"), "not installed");
    assert!(description.contains("Default agent: claude-code."));
    assert_eq!(spawn["inputSchema"]["properties"]["agent"]["enum"], json!(["claude-code"]));
    assert_eq!(spawn["inputSchema"]["required"], json!(["prompt", "request_id"]));
    // Without a default the agent must be named.
    let tools = tools_for(Some(&me(true)), None);
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["prompt", "request_id", "agent"]));
}

#[test]
fn the_wait_tool_carries_the_shared_guidance() {
    let tools = fixed_tools();
    let wait = tools.iter().find(|t| t["name"] == "oximux_wait_agents").unwrap();
    let description = wait["description"].as_str().unwrap();
    assert!(description.contains(WAIT_GUIDANCE));
    assert!(description.contains(ETIQUETTE));
}

/// Shouted words and imperative boilerplate read to a model like an
/// injection; none may creep into what these tools say.
#[test]
fn the_wording_is_calm() {
    let mut texts = vec![INSTRUCTIONS.to_string(), WAIT_GUIDANCE.to_string(), ETIQUETTE.to_string()];
    texts.extend(tools_for(Some(&me(true)), Some("claude-code")).iter().map(|t| t["description"].to_string()));
    for text in texts {
        for word in text.split(|c: char| !c.is_ascii_alphabetic()) {
            assert!(!(word.len() >= 3 && word.chars().all(|c| c.is_ascii_uppercase())), "{word:?} in {text}");
        }
        let lower = text.to_lowercase();
        for phrase in ["you must", "important", "critical", "!!"] {
            assert!(!lower.contains(phrase), "{phrase:?} in {text}");
        }
    }
}

fn frame(seq: u64, event: ThreadEvent) -> HostEvent {
    HostEvent::new("c1", seq, &event, SessionStatusWire { last_seq: seq, awaiting_permission: false }).unwrap()
}

fn prompt() -> ThreadEvent {
    ThreadEvent::UserMessage { text: "go".into(), images: vec![] }
}

fn ended(is_error: bool) -> ThreadEvent {
    ThreadEvent::TurnEnded { result: None, usage: None, is_error, turn_diff: None }
}

#[test]
fn a_wait_reads_the_latest_turn_from_the_events() {
    // Not started: no prompt and no turn yet is not "over".
    assert_eq!(scan(&[]), (None, 0));
    // Prompted and running.
    assert_eq!(scan(&[frame(1, prompt())]), (None, 1));
    // Prompted and finished, or failed.
    assert_eq!(scan(&[frame(1, prompt()), frame(5, ended(false))]), (Some(false), 5));
    assert_eq!(scan(&[frame(1, prompt()), frame(5, ended(true))]), (Some(true), 5));
    // A follow-up prompt after the turn ended: running again.
    assert_eq!(scan(&[frame(1, prompt()), frame(5, ended(false)), frame(6, prompt())]), (None, 6));
    // The prompt aged out of the ring but its end did not.
    assert_eq!(scan(&[frame(9, ended(false))]), (Some(false), 9));
}

#[test]
fn parked_and_ended_children_are_answered_from_lineage() {
    assert_eq!(from_lineage(LineageStateWire::Parked), Some(Outcome::IDLE));
    assert_eq!(from_lineage(LineageStateWire::Ended), Some(Outcome::ENDED));
    assert_eq!(from_lineage(LineageStateWire::Active), None);
    assert_eq!(from_lineage(LineageStateWire::Reserved), None);
}

#[test]
fn a_wait_is_bounded() {
    assert_eq!(wait_timeout(&json!({})), Duration::from_millis(MAX_WAIT_MS));
    assert_eq!(wait_timeout(&json!({ "timeout_ms": 600_000 })), Duration::from_millis(MAX_WAIT_MS));
    assert_eq!(wait_timeout(&json!({ "timeout_ms": 5 })), Duration::from_secs(1));
    assert_eq!(wait_timeout(&json!({ "timeout_ms": 20_000 })), Duration::from_secs(20));
}

#[test]
fn a_long_summary_is_cut_on_a_char_boundary() {
    let text = "é".repeat(10);
    assert_eq!(truncate_chars(&text, 4), ("éééé", 6));
    assert_eq!(truncate_chars("short", 10), ("short", 0));
}
