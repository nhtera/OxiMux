//! The computer-use declaration merges into a spec other writers share: it
//! must keep their declarations, and a posture-locked child gets no screen.

use oximux_agents::thread::launch_posture::SpawnPosture;
use oximux_agents::thread::{ChatBackend, ConnectSpec, McpServerSpec};
use oximux_computer_use::mcp::Declaration;

use super::apply_declaration;

fn declaration() -> Declaration {
    Declaration {
        server: Some(McpServerSpec::new("oximux-computer-use", "/bin/driver")),
        disallowed_tools: vec!["mcp__oximux-computer-use__replay_trajectory".into()],
        hook_settings: r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"gate"}]}]}}"#
            .into(),
    }
}

fn spec(posture: SpawnPosture) -> ConnectSpec {
    let mut s = ConnectSpec::for_backend(&ChatBackend::stream_json(), "/tmp".into(), None, None, None, None);
    s.posture = posture;
    // What the other writers put there first.
    s.set_env("OXIMUX_SESSION_TOKEN", "credential").unwrap();
    s.add_mcp_server(McpServerSpec::new("oximux", "/bin/orchestrate")).unwrap();
    s.merge_settings(&serde_json::json!({"fastMode": true})).unwrap();
    s
}

#[test]
fn computer_use_keeps_every_other_writers_declarations() {
    let mut s = spec(SpawnPosture::Default);
    apply_declaration(&mut s, declaration()).unwrap();
    let names: Vec<&str> = s.mcp_servers.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["oximux", "oximux-computer-use"], "neither server clobbered");
    assert_eq!(s.env, [("OXIMUX_SESSION_TOKEN".to_string(), "credential".to_string())]);
    let settings: serde_json::Value = serde_json::from_str(s.settings_json.as_deref().unwrap()).unwrap();
    assert_eq!(settings["fastMode"], true, "fast mode survives");
    assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 1, "the gate hook is in");
    assert_eq!(s.disallowed_tools.len(), 1);
}

#[test]
fn a_review_child_gets_the_gate_hook_but_not_the_screen() {
    let mut s = spec(SpawnPosture::Review);
    apply_declaration(&mut s, declaration()).unwrap();
    assert!(
        s.mcp_servers.iter().all(|m| m.name != "oximux-computer-use"),
        "no computer-use server for a reviewer"
    );
    let settings: serde_json::Value = serde_json::from_str(s.settings_json.as_deref().unwrap()).unwrap();
    assert!(settings["hooks"]["PreToolUse"].is_array(), "the shell is still gated");
    assert_eq!(s.env.len(), 1, "the credential survives");
}
