use serde::Serialize;

use crate::cli::workspace::PolicyConfig;

/// Generate a random u64 from OS entropy (no extra deps).
fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// Deliberation request matching the orchestrator's POST /deliberation JSON contract.
#[derive(Debug, Serialize)]
pub struct DeliberationRequest {
    pub room_id: String,
    /// Stable conversation key (the thread id) shared across a thread's turns —
    /// the `room_id` above carries a per-run nonce, so this is what lets the
    /// agents resume the same claude session turn to turn. `None` for ad-hoc runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub user_query: String,
    pub deliberation_rounds: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_names: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Human-in-the-loop tools offered to the agents. The TUI ships `ask_user`
    /// so an agent can ask the operator a clarifying question mid-deliberation
    /// (and the TUI surfaces it + posts the answer back).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_tools: Option<Vec<crate::agents::UserToolDefinition>>,
    /// The conversation as a role-tagged message array (the native form). The
    /// agent renders it per session-resume state; supersedes `user_query`.
    /// Empty on non-thread / ad-hoc runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<crate::conversation::Message>,
}

/// The `ask_user` HITL tool: an agent asks the operator a clarifying question,
/// optionally offering a short list of `options` for a quick choice (they may
/// also answer freely). Request-response — the agent blocks on the answer (up to
/// its finalization reserve).
pub fn ask_user_tool() -> crate::agents::UserToolDefinition {
    crate::agents::UserToolDefinition {
        name: "ask_user".to_string(),
        description: "Ask the human operator a clarifying question when you \
             genuinely need their input to proceed — ambiguous requirements, a \
             decision only they can make, or missing context. Optionally include \
             a short `options` list for a quick choice; they may also answer \
             freely. Use sparingly: only when their answer materially changes the \
             result."
            .to_string(),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the operator."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional short list of choices to offer."
                }
            },
            "required": ["question"]
        })),
        strict: Some(false),
    }
}

/// Build a `DeliberationRequest` from a raw policy_id hash (ad-hoc run).
///
/// Used when the user passes a 64-char hex policy_id directly via `--policy`,
/// bypassing local policy lookup. Uses defaults for rounds/effort since
/// the orchestrator owns the policy config.
pub fn build_request_raw_policy_id(policy_id: &str, task: &str) -> DeliberationRequest {
    let nonce: u64 = rand_u64();
    let room_id = format!("adhoc_{nonce:016x}");

    DeliberationRequest {
        room_id,
        conversation_id: None,
        user_query: task.to_string(),
        deliberation_rounds: 3,
        agent_names: None,
        policy_id: Some(policy_id.to_string()),
        effort: None,
        scope: None,
        timeout_seconds: None,
        user_tools: Some(vec![ask_user_tool()]),
        messages: Vec::new(),
    }
}

/// Build a `DeliberationRequest` from workspace policy config.
///
/// - Static policies (`agents` field) → sends `agent_names` directly.
/// - Role-based policies (`roles` field) → computes `policy_id` (content hash)
///   and sends it to the orchestrator for server-side agent resolution.
pub fn build_request(
    room_name: &str,
    policy: &PolicyConfig,
    task: &str,
) -> Result<DeliberationRequest, String> {
    let (agent_names, policy_id) = if let Some(agents) = &policy.agents {
        if agents.len() < 2 {
            return Err("policy must specify at least two agents for deliberation".to_string());
        }
        (Some(agents.clone()), None)
    } else if policy.roles.is_some() {
        let id = policy.policy_id();
        (None, Some(id))
    } else {
        return Err("policy must specify either agents or roles".to_string());
    };

    // Forward the policy SLA's whole-job budget verbatim as the client-side
    // wall-clock deadline. `job_timeout_secs` is the JIT contract — "when
    // will this query be guaranteed to answer" — so adding an overhead
    // buffer here would silently extend that contract; any overhead budget
    // (e.g. HITL release time) must be carved out of the whole-job envelope
    // itself, not bolted on top. `PolicySla::job_timeout()` also maps the
    // `0` sentinel to `None` so we never forward a fake deadline.
    let timeout_seconds = policy
        .sla
        .as_ref()
        .and_then(|sla| sla.job_timeout())
        .map(|d| d.as_secs());

    // Generate a unique room_id per run to avoid 409 conflicts.
    // Format: {room_name}_{nonce} — human-readable prefix + random suffix.
    let nonce: u64 = rand_u64();
    let room_id = format!("{room_name}_{nonce:016x}");

    Ok(DeliberationRequest {
        room_id,
        // The pre-nonce room name is the stable thread key — carry it so every
        // turn of the thread resumes the same claude session.
        conversation_id: Some(room_name.to_string()),
        user_query: task.to_string(),
        deliberation_rounds: policy.max_rounds,
        agent_names,
        policy_id,
        effort: Some(policy.effort),
        scope: None,
        timeout_seconds,
        user_tools: Some(vec![ask_user_tool()]),
        messages: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::workspace::PolicyConfig;
    use crate::scheduling::PolicySla;

    fn static_policy() -> PolicyConfig {
        PolicyConfig {
            agents: Some(vec!["agent-a".into(), "agent-b".into()]),
            roles: None,
            max_rounds: 3,
            effort: 0.85,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        }
    }

    fn roles_policy() -> PolicyConfig {
        PolicyConfig {
            agents: None,
            roles: Some(vec![crate::cli::workspace::RoleConfig {
                role: "reviewer".into(),
                count: 2,
                capabilities: vec!["lang:rust".into()],
                context: None,
                pinned_agents: None,
                moderator: false,
            }]),
            max_rounds: 3,
            effort: 0.85,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        }
    }

    #[test]
    fn requests_offer_the_ask_user_tool() {
        let req = build_request("r", &static_policy(), "q").unwrap();
        let tools = req.user_tools.expect("ships ask_user");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ask_user");
        // question required, options optional — expressed in the JSON schema.
        let params = tools[0].parameters.as_ref().unwrap();
        assert_eq!(params["required"][0], "question");
        assert!(params["properties"]["options"].is_object());
        // Ad-hoc requests carry it too.
        assert!(
            build_request_raw_policy_id("p", "q")
                .user_tools
                .is_some_and(|t| t[0].name == "ask_user")
        );
    }

    #[test]
    fn build_request_static_agents() {
        let req = build_request("my-room", &static_policy(), "audit this code").unwrap();
        assert!(
            req.room_id.starts_with("my-room_"),
            "room_id should be prefixed with room name, got: {}",
            req.room_id
        );
        assert_eq!(req.user_query, "audit this code");
        assert_eq!(req.deliberation_rounds, 3);
        assert_eq!(
            req.agent_names.as_deref(),
            Some(&["agent-a".to_string(), "agent-b".to_string()][..])
        );
        assert!(req.policy_id.is_none(), "static should not send policy_id");
        assert_eq!(req.effort, Some(0.85));
        assert!(req.scope.is_none());
        assert!(req.timeout_seconds.is_none());
    }

    #[test]
    fn build_request_single_agent_rejected() {
        let mut policy = static_policy();
        policy.agents = Some(vec!["only-one".into()]);
        let err = build_request("room", &policy, "task").unwrap_err();
        assert!(
            err.contains("at least two"),
            "expected min-agents error, got: {err}"
        );
    }

    #[test]
    fn build_request_roles_sends_policy_id() {
        let policy = roles_policy();
        let req = build_request("room", &policy, "task").unwrap();
        assert!(
            req.agent_names.is_none(),
            "role-based should not send agent_names"
        );
        assert!(req.policy_id.is_some(), "role-based should send policy_id");
        assert_eq!(req.policy_id.unwrap(), policy.policy_id());
    }

    #[test]
    fn build_request_effort_passthrough() {
        let mut policy = static_policy();
        policy.effort = 0.42;
        let req = build_request("room", &policy, "task").unwrap();
        assert_eq!(req.effort, Some(0.42));
    }

    #[test]
    fn build_request_sla_maps_to_timeout() {
        let mut policy = static_policy();
        policy.sla = Some(PolicySla {
            job_timeout_secs: 600,
            response_sla_secs: None,
            max_tokens: None,
        });
        let req = build_request("room", &policy, "task").unwrap();
        // `job_timeout_secs` is the JIT contract and is forwarded verbatim —
        // no buffer, no scaling.
        assert_eq!(req.timeout_seconds, Some(600));
    }

    #[test]
    fn build_request_timeout_does_not_scale_with_max_rounds() {
        // Regression guard: `job_timeout_secs` is the whole-job wall-clock
        // envelope, NOT a per-phase budget. Changing `max_rounds` must not
        // change the client-side timeout.
        let sla = PolicySla {
            job_timeout_secs: 300,
            response_sla_secs: None,
            max_tokens: None,
        };

        let mut policy_1 = static_policy();
        policy_1.max_rounds = 1;
        policy_1.sla = Some(sla.clone());

        let mut policy_10 = static_policy();
        policy_10.max_rounds = 10;
        policy_10.sla = Some(sla);

        let req_1 = build_request("r1", &policy_1, "task").unwrap();
        let req_10 = build_request("r10", &policy_10, "task").unwrap();

        assert_eq!(req_1.timeout_seconds, Some(300));
        assert_eq!(req_10.timeout_seconds, Some(300));
        assert_eq!(req_1.timeout_seconds, req_10.timeout_seconds);
    }

    #[test]
    fn build_request_timeout_is_none_when_sla_job_timeout_is_zero() {
        // The `0` sentinel on `job_timeout_secs` means "no explicit budget"
        // and must be surfaced as `None`, not forwarded as a real deadline.
        // Workspace validation rejects a zero SLA at load time, but other
        // code paths (raw API payloads) could still hit `build_request`
        // with a zero value, so the conversion must be defensive here.
        let mut policy = static_policy();
        policy.sla = Some(PolicySla {
            job_timeout_secs: 0,
            response_sla_secs: None,
            max_tokens: None,
        });
        let req = build_request("room", &policy, "task").unwrap();
        assert_eq!(req.timeout_seconds, None);
    }

    #[test]
    fn build_request_timeout_handles_u64_max_without_overflow() {
        // Regression guard: the previous `x + x/10` overhead buffer could
        // overflow on extreme values. The new implementation forwards the
        // value verbatim, so `u64::MAX` must round-trip unchanged.
        let mut policy = static_policy();
        policy.sla = Some(PolicySla {
            job_timeout_secs: u64::MAX,
            response_sla_secs: None,
            max_tokens: None,
        });
        let req = build_request("room", &policy, "task").unwrap();
        assert_eq!(req.timeout_seconds, Some(u64::MAX));
    }

    #[test]
    fn build_request_serializes_to_expected_json() {
        let req = build_request("room", &static_policy(), "task").unwrap();
        let json = serde_json::to_value(&req).unwrap();
        assert!(json["room_id"].as_str().unwrap().starts_with("room_"));
        assert_eq!(json["agent_names"][0], "agent-a");
        // effort should be serialized under its new key
        assert!(json.get("effort").is_some(), "effort field must be present");
        let effort_val = json["effort"].as_f64().unwrap();
        assert!(
            (effort_val - 0.85).abs() < 1e-6,
            "effort should be ~0.85, got {effort_val}"
        );
        // Optional None fields should be absent
        assert!(json.get("policy_id").is_none());
        assert!(json.get("scope").is_none());
        assert!(json.get("timeout_seconds").is_none());
    }

    #[test]
    fn build_request_roles_serializes_policy_id() {
        let req = build_request("room", &roles_policy(), "task").unwrap();
        let json = serde_json::to_value(&req).unwrap();
        assert!(
            json.get("agent_names").is_none(),
            "role-based should omit agent_names"
        );
        assert!(
            json["policy_id"].is_string(),
            "role-based should have policy_id"
        );
    }

    #[test]
    fn build_request_raw_policy_id_creates_adhoc_room() {
        let hash = "a".repeat(64);
        let req = build_request_raw_policy_id(&hash, "audit this");
        assert!(
            req.room_id.starts_with("adhoc_"),
            "room_id should start with 'adhoc_', got: {}",
            req.room_id
        );
        assert_eq!(req.user_query, "audit this");
        assert_eq!(req.policy_id.as_deref(), Some(hash.as_str()));
        assert!(req.agent_names.is_none());
        assert_eq!(req.deliberation_rounds, 3);
        assert!(req.effort.is_none());
    }

    #[test]
    fn build_request_adhoc_named_policy() {
        let policy = roles_policy();
        let req = build_request("adhoc", &policy, "review this").unwrap();
        assert!(req.room_id.starts_with("adhoc_"));
        assert!(req.policy_id.is_some());
        assert_eq!(req.policy_id.unwrap(), policy.policy_id());
    }
}
