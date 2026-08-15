//! Integration coverage for the shared `read_proposal` / `read_critiques`
//! positional-argument parsing exposed through `extract_python_tool_calls`.
//! Both tools route through one helper that differs only by the id key
//! (`agent_id` vs `target_agent_id`); these assert that split end-to-end.

use llm_repair::extraction::extract_python_tool_calls;
use serde_json::Value;

fn args_of(input: &str, tool: &str) -> Value {
    let calls = extract_python_tool_calls(input);
    let call = calls
        .iter()
        .find(|c| c.function.name == tool)
        .unwrap_or_else(|| panic!("no {tool} call parsed from {input:?}"));
    serde_json::from_str(&call.function.arguments).unwrap()
}

#[test]
fn round_and_id_positional_parse_uses_the_tool_specific_id_key() {
    let p = args_of(r#"read_proposal(2, "Alice")"#, "read_proposal");
    assert_eq!(p["round"], 2);
    assert_eq!(p["agent_id"], "Alice");

    let c = args_of(r#"read_critiques(2, "Bob")"#, "read_critiques");
    assert_eq!(c["round"], 2);
    assert_eq!(c["target_agent_id"], "Bob");
}

#[test]
fn single_positional_maps_to_the_tool_specific_id_key() {
    assert_eq!(
        args_of(r#"read_proposal("Alice")"#, "read_proposal")["agent_id"],
        "Alice"
    );
    assert_eq!(
        args_of(r#"read_critiques("Bob")"#, "read_critiques")["target_agent_id"],
        "Bob"
    );
}

#[test]
fn json_object_args_win_over_positional_parsing() {
    let p = args_of(
        r#"read_proposal({"round": 3, "agent_id": "Zoe"})"#,
        "read_proposal",
    );
    assert_eq!(p["round"], 3);
    assert_eq!(p["agent_id"], "Zoe");
}
